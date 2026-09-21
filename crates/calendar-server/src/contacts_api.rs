//! Contacts application API (docs/CARDDAV_DESIGN.md): personal address books
//! are real rows the caller owns; the tenant directory is a virtual,
//! read-only book (id derived deterministically, never stored) listing every
//! tenant member. Same read/write token scopes as the rest of the API — no
//! new scope names.

use crate::{AppError, AppState, require_csrf, resolve_auth};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::IntoResponse,
    routing::{get, post},
};
use base64::Engine;
use calendar_db::{self as db};
use serde_json::json;
use uuid::Uuid;

#[derive(serde::Serialize, utoipa::ToSchema)]
struct ContactEmailView {
    email: String,
    kind: Option<String>,
    is_primary: bool,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct ContactTelView {
    number: String,
    kind: Option<String>,
    is_mobile: bool,
    is_primary: bool,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct ContactMemberView {
    contact_id: Option<Uuid>,
    user_id: Option<Uuid>,
    full_name: String,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct ContactView {
    id: Uuid,
    address_book_id: Uuid,
    uid: String,
    /// "individual" or "group".
    kind: String,
    full_name: String,
    given_name: Option<String>,
    family_name: Option<String>,
    org: Option<String>,
    title: Option<String>,
    street_address: Option<String>,
    locality: Option<String>,
    region: Option<String>,
    postal_code: Option<String>,
    country: Option<String>,
    has_photo: bool,
    etag: String,
    updated_at: chrono::DateTime<chrono::Utc>,
    emails: Vec<ContactEmailView>,
    tels: Vec<ContactTelView>,
    /// Group membership only (kind = "group"); empty otherwise.
    members: Vec<ContactMemberView>,
}

fn contact_json(
    c: &db::contacts::ContactRow,
    emails: &[db::contacts::ContactEmailRow],
    tels: &[db::contacts::ContactTelRow],
    members: &[db::contacts::ResolvedMember],
) -> serde_json::Value {
    json!({
        "id": c.id, "address_book_id": c.address_book_id, "uid": c.uid, "kind": c.kind,
        "full_name": c.full_name, "given_name": c.given_name, "family_name": c.family_name,
        "org": c.org, "title": c.title,
        "street_address": c.street_address, "locality": c.locality, "region": c.region,
        "postal_code": c.postal_code, "country": c.country,
        "has_photo": c.photo_mime.is_some(), "etag": c.etag, "updated_at": c.updated_at,
        "emails": emails.iter().map(|e| json!({
            "email": e.email, "kind": e.kind, "is_primary": e.is_primary,
        })).collect::<Vec<_>>(),
        "tels": tels.iter().map(|t| json!({
            "number": t.number, "kind": t.kind, "is_mobile": t.is_mobile, "is_primary": t.is_primary,
        })).collect::<Vec<_>>(),
        "members": members.iter().map(|m| json!({
            "contact_id": m.contact_id, "user_id": m.user_id, "full_name": m.full_name,
        })).collect::<Vec<_>>(),
    })
}

fn directory_entry_json(e: &db::contacts::DirectoryEntry) -> serde_json::Value {
    json!({
        "id": e.user_id, "user_id": e.user_id, "kind": "individual", "directory": true,
        "full_name": e.display_name.clone().unwrap_or_else(|| e.username.clone()),
        "emails": [{"email": e.email, "kind": "work", "is_primary": true}],
        "tels": [], "members": [], "updated_at": e.updated_at,
    })
}

/// Group members addressed by contact id or tenant user id, turned into the
/// `urn:uuid:{uid}` MEMBER values the vCard writer/CardDAV resolution expect
/// (calendar_db::contacts::upsert_contact resolves the same scheme back).
async fn group_member_uris(
    pool: &sqlx::PgPool,
    contact_ids: &[Uuid],
    user_ids: &[Uuid],
) -> Result<Vec<String>, AppError> {
    let mut uris = Vec::with_capacity(contact_ids.len() + user_ids.len());
    for id in contact_ids {
        let member = db::contacts::get_contact(pool, *id)
            .await
            .map_err(|_| AppError::bad_request(format!("member contact {id} not found")))?;
        uris.push(format!("urn:uuid:{}", member.uid));
    }
    for id in user_ids {
        uris.push(format!("urn:uuid:{id}"));
    }
    Ok(uris)
}

// ============ address books ============

#[derive(serde::Serialize, utoipa::ToSchema)]
struct AddressBookSummary {
    id: Uuid,
    slug: String,
    name: String,
    /// "personal" for owned books, "directory" for the virtual tenant book.
    kind: &'static str,
    ctag: i64,
}

#[utoipa::path(
    get,
    path = "/api/addressbooks",
    responses(
        (status = 200, description = "the caller's books plus the virtual tenant directory", body = Vec<AddressBookSummary>),
    )
)]
async fn list_address_books(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let mut books = db::contacts::list_address_books_for_user(&pool, auth.user.id).await?;
    if books.is_empty() {
        books
            .push(db::contacts::ensure_default_address_book(&pool, tenant_id, auth.user.id).await?);
    }
    let mut out: Vec<serde_json::Value> = books
        .iter()
        .map(|b| json!({"id": b.id, "slug": b.slug, "name": b.name, "kind": "personal", "ctag": b.ctag}))
        .collect();
    out.push(json!({
        "id": db::contacts::directory_book_id(tenant_id), "slug": db::contacts::DIRECTORY_SLUG,
        "name": "Directory", "kind": "directory", "ctag": 0,
    }));
    Ok(Json(json!(out)))
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct AddressBookBody {
    slug: String,
    name: String,
}

#[utoipa::path(
    post,
    path = "/api/addressbooks",
    request_body = AddressBookBody,
    responses(
        (status = 201, description = "created", body = AddressBookRowView),
        (status = 400, description = "invalid or reserved slug"),
    )
)]
async fn create_address_book(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<AddressBookBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    calendar_core::validate_slug(&body.slug).map_err(|e| AppError::bad_request(e.to_string()))?;
    if body.slug == db::contacts::DIRECTORY_SLUG {
        return Err(AppError::bad_request("that slug is reserved"));
    }
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let row =
        db::contacts::create_address_book(&pool, tenant_id, auth.user.id, &body.slug, &body.name)
            .await?;
    Ok((axum::http::StatusCode::CREATED, Json(json!(row))))
}

/// Mirrors calendar-db's AddressBookRow (its serde shape is the create/rename
/// response; the list endpoint returns summaries).
#[derive(serde::Serialize, utoipa::ToSchema)]
struct AddressBookRowView {
    id: Uuid,
    tenant_id: Uuid,
    owner_user_id: Uuid,
    slug: String,
    name: String,
    ctag: i64,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct RenameBody {
    name: String,
}

#[utoipa::path(
    patch,
    path = "/api/addressbooks/{id}",
    params(("id" = Uuid, Path, description = "address book id (owner only)")),
    request_body = RenameBody,
    responses(
        (status = 200, description = "renamed", body = AddressBookRowView),
        (status = 403, description = "not owner"),
        (status = 404, description = "absent"),
    )
)]
async fn patch_address_book(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<RenameBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let row = db::contacts::get_address_book(&pool, id)
        .await
        .map_err(|_| AppError::NotFound)?;
    if row.owner_user_id != auth.user.id {
        return Err(AppError::Forbidden);
    }
    let row = db::contacts::rename_address_book(&pool, id, &body.name).await?;
    Ok(Json(json!(row)))
}

#[utoipa::path(
    delete,
    path = "/api/addressbooks/{id}",
    params(("id" = Uuid, Path, description = "address book id (owner only)")),
    responses(
        (status = 200, description = "deleted (soft)", body = crate::OkView),
        (status = 403, description = "not owner"),
        (status = 404, description = "absent"),
    )
)]
async fn delete_address_book(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let row = db::contacts::get_address_book(&pool, id)
        .await
        .map_err(|_| AppError::NotFound)?;
    if row.owner_user_id != auth.user.id {
        return Err(AppError::Forbidden);
    }
    db::contacts::soft_delete_address_book(&pool, id).await?;
    Ok(Json(json!({"ok": true})))
}

// ============ contacts ============

#[utoipa::path(
    get,
    path = "/api/addressbooks/{id}/contacts",
    params(("id" = Uuid, Path, description = "address book id (the tenant directory id lists every member)")),
    responses(
        (status = 200, description = "contacts, or directory entries for the virtual directory book", body = Vec<ContactView>),
        (status = 403, description = "not owner"),
        (status = 404, description = "absent"),
    )
)]
async fn list_contacts(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(address_book_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    if address_book_id == db::contacts::directory_book_id(tenant_id) {
        let entries = db::contacts::directory_entries(&pool, tenant_id).await?;
        return Ok(Json(json!(
            entries.iter().map(directory_entry_json).collect::<Vec<_>>()
        )));
    }
    let book = db::contacts::get_address_book(&pool, address_book_id)
        .await
        .map_err(|_| AppError::NotFound)?;
    if book.owner_user_id != auth.user.id {
        return Err(AppError::Forbidden);
    }
    let contacts = db::contacts::list_contacts(&pool, address_book_id).await?;
    let mut out = Vec::with_capacity(contacts.len());
    for c in contacts {
        let emails = db::contacts::list_emails(&pool, c.id).await?;
        let tels = db::contacts::list_tels(&pool, c.id).await?;
        let members = if c.kind == "group" {
            db::contacts::resolved_group_members(&pool, c.id).await?
        } else {
            vec![]
        };
        out.push(contact_json(&c, &emails, &tels, &members));
    }
    Ok(Json(json!(out)))
}

#[derive(serde::Deserialize, Default, utoipa::ToSchema)]
struct ContactBody {
    /// "individual" (default) or "group". Ignored on PATCH — a contact's
    /// kind doesn't change after creation.
    kind: Option<String>,
    full_name: String,
    given_name: Option<String>,
    family_name: Option<String>,
    org: Option<String>,
    title: Option<String>,
    #[serde(default)]
    emails: Vec<EmailBody>,
    #[serde(default)]
    tels: Vec<TelBody>,
    /// Group membership (kind="group" only): other contacts in the same
    /// book, plus/or tenant directory users. Replaces the whole set.
    #[serde(default)]
    member_contact_ids: Vec<Uuid>,
    #[serde(default)]
    member_user_ids: Vec<Uuid>,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct EmailBody {
    email: String,
    kind: Option<String>,
    #[serde(default)]
    is_primary: bool,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct TelBody {
    number: String,
    kind: Option<String>,
    #[serde(default)]
    is_mobile: bool,
    #[serde(default)]
    is_primary: bool,
}

#[utoipa::path(
    post,
    path = "/api/addressbooks/{id}/contacts",
    params(("id" = Uuid, Path, description = "address book id (owner only)")),
    request_body = ContactBody,
    responses(
        (status = 201, description = "created", body = ContactView),
        (status = 400, description = "validation error or unknown member"),
        (status = 403, description = "not owner"),
        (status = 404, description = "absent"),
    )
)]
async fn create_contact(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(address_book_id): Path<Uuid>,
    Json(body): Json<ContactBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let book = db::contacts::get_address_book(&pool, address_book_id)
        .await
        .map_err(|_| AppError::NotFound)?;
    if book.owner_user_id != auth.user.id {
        return Err(AppError::Forbidden);
    }
    if body.full_name.trim().is_empty() {
        return Err(AppError::bad_request("full_name is required"));
    }
    let kind = match body.kind.as_deref() {
        None | Some("individual") => "individual",
        Some("group") => "group",
        Some(_) => return Err(AppError::bad_request("kind must be individual or group")),
    };
    let uid = Uuid::new_v4().to_string();
    let emails: Vec<db::contacts::NewEmail> = body
        .emails
        .iter()
        .map(|e| db::contacts::NewEmail {
            email: e.email.clone(),
            kind: e.kind.clone(),
            is_primary: e.is_primary,
        })
        .collect();
    let tels: Vec<db::contacts::NewTel> = body
        .tels
        .iter()
        .map(|t| db::contacts::NewTel {
            number: t.number.clone(),
            kind: t.kind.clone(),
            is_mobile: t.is_mobile,
            is_primary: t.is_primary,
        })
        .collect();
    let group_members =
        group_member_uris(&pool, &body.member_contact_ids, &body.member_user_ids).await?;
    let raw_vcard = calendar_carddav::vcard::write_vcard_3_0(
        &uid,
        kind,
        &body.full_name,
        body.given_name.as_deref(),
        body.family_name.as_deref(),
        body.org.as_deref(),
        body.title.as_deref(),
        &emails,
        &tels,
        &group_members,
    );
    let contact = db::contacts::upsert_contact(
        &pool,
        address_book_id,
        &db::contacts::NewContact {
            uid,
            kind: kind.into(),
            full_name: body.full_name,
            given_name: body.given_name,
            family_name: body.family_name,
            org: body.org,
            title: body.title,
            street_address: None,
            locality: None,
            region: None,
            postal_code: None,
            country: None,
            raw_vcard,
            emails,
            tels,
            group_members,
        },
    )
    .await?;
    let emails = db::contacts::list_emails(&pool, contact.id).await?;
    let tels = db::contacts::list_tels(&pool, contact.id).await?;
    let members = db::contacts::resolved_group_members(&pool, contact.id).await?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(contact_json(&contact, &emails, &tels, &members)),
    ))
}

#[utoipa::path(
    get,
    path = "/api/contacts/{id}",
    params(("id" = Uuid, Path, description = "contact id")),
    responses(
        (status = 200, description = "contact with emails, tels and group members", body = ContactView),
        (status = 403, description = "not the book owner"),
        (status = 404, description = "absent"),
    )
)]
async fn get_contact(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let contact = db::contacts::get_contact(&pool, id)
        .await
        .map_err(|_| AppError::NotFound)?;
    let book = db::contacts::get_address_book(&pool, contact.address_book_id)
        .await
        .map_err(|_| AppError::NotFound)?;
    if book.owner_user_id != auth.user.id {
        return Err(AppError::Forbidden);
    }
    let emails = db::contacts::list_emails(&pool, contact.id).await?;
    let tels = db::contacts::list_tels(&pool, contact.id).await?;
    let members = db::contacts::resolved_group_members(&pool, contact.id).await?;
    Ok(Json(contact_json(&contact, &emails, &tels, &members)))
}

#[utoipa::path(
    patch,
    path = "/api/contacts/{id}",
    params(("id" = Uuid, Path, description = "contact id")),
    request_body = ContactBody,
    responses(
        (status = 200, description = "updated", body = ContactView),
        (status = 403, description = "not the book owner"),
        (status = 404, description = "absent"),
    )
)]
async fn patch_contact(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<ContactBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let current = db::contacts::get_contact(&pool, id)
        .await
        .map_err(|_| AppError::NotFound)?;
    let book = db::contacts::get_address_book(&pool, current.address_book_id)
        .await
        .map_err(|_| AppError::NotFound)?;
    if book.owner_user_id != auth.user.id {
        return Err(AppError::Forbidden);
    }
    let emails: Vec<db::contacts::NewEmail> = body
        .emails
        .iter()
        .map(|e| db::contacts::NewEmail {
            email: e.email.clone(),
            kind: e.kind.clone(),
            is_primary: e.is_primary,
        })
        .collect();
    let tels: Vec<db::contacts::NewTel> = body
        .tels
        .iter()
        .map(|t| db::contacts::NewTel {
            number: t.number.clone(),
            kind: t.kind.clone(),
            is_mobile: t.is_mobile,
            is_primary: t.is_primary,
        })
        .collect();
    let group_members = if current.kind == "group" {
        group_member_uris(&pool, &body.member_contact_ids, &body.member_user_ids).await?
    } else {
        vec![]
    };
    let raw_vcard = calendar_carddav::vcard::write_vcard_3_0(
        &current.uid,
        &current.kind,
        &body.full_name,
        body.given_name.as_deref(),
        body.family_name.as_deref(),
        body.org.as_deref(),
        body.title.as_deref(),
        &emails,
        &tels,
        &group_members,
    );
    let contact = db::contacts::upsert_contact(
        &pool,
        current.address_book_id,
        &db::contacts::NewContact {
            uid: current.uid,
            kind: current.kind,
            full_name: body.full_name,
            given_name: body.given_name,
            family_name: body.family_name,
            org: body.org,
            title: body.title,
            street_address: current.street_address,
            locality: current.locality,
            region: current.region,
            postal_code: current.postal_code,
            country: current.country,
            raw_vcard,
            emails,
            tels,
            group_members,
        },
    )
    .await?;
    let emails = db::contacts::list_emails(&pool, contact.id).await?;
    let tels = db::contacts::list_tels(&pool, contact.id).await?;
    let members = db::contacts::resolved_group_members(&pool, contact.id).await?;
    Ok(Json(contact_json(&contact, &emails, &tels, &members)))
}

#[utoipa::path(
    delete,
    path = "/api/contacts/{id}",
    params(("id" = Uuid, Path, description = "contact id")),
    responses(
        (status = 200, description = "deleted (soft, CardDAV-visible tombstone)", body = crate::OkView),
        (status = 403, description = "not the book owner"),
        (status = 404, description = "absent"),
    )
)]
async fn delete_contact(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let contact = db::contacts::get_contact(&pool, id)
        .await
        .map_err(|_| AppError::NotFound)?;
    let book = db::contacts::get_address_book(&pool, contact.address_book_id)
        .await
        .map_err(|_| AppError::NotFound)?;
    if book.owner_user_id != auth.user.id {
        return Err(AppError::Forbidden);
    }
    db::contacts::soft_delete_contact(&pool, id).await?;
    Ok(Json(json!({"ok": true})))
}

// ============ photo ============

#[utoipa::path(
    get,
    path = "/api/contacts/{id}/photo",
    params(("id" = Uuid, Path, description = "contact id")),
    responses(
        (status = 200, description = "photo bytes with the stored MIME type"),
        (status = 404, description = "absent or no photo"),
    )
)]
async fn get_photo(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let contact = db::contacts::get_contact(&pool, id)
        .await
        .map_err(|_| AppError::NotFound)?;
    let book = db::contacts::get_address_book(&pool, contact.address_book_id)
        .await
        .map_err(|_| AppError::NotFound)?;
    if book.owner_user_id != auth.user.id {
        return Err(AppError::Forbidden);
    }
    let (data, mime) = db::contacts::get_photo(&pool, id)
        .await
        .map_err(|_| AppError::NotFound)?;
    Ok(([(axum::http::header::CONTENT_TYPE, mime)], data))
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct PhotoBody {
    content_type: String,
    /// base64-encoded bytes; capped like event attachments (ADR-010).
    data: String,
}

#[utoipa::path(
    put,
    path = "/api/contacts/{id}/photo",
    params(("id" = Uuid, Path, description = "contact id")),
    request_body = PhotoBody,
    responses(
        (status = 200, description = "stored", body = crate::OkView),
        (status = 400, description = "invalid base64 or over the byte cap"),
        (status = 403, description = "not the book owner"),
        (status = 404, description = "absent"),
    )
)]
async fn put_photo(
    State(AppState { pool, config, .. }): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<PhotoBody>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    require_csrf(&auth, &headers)?;
    let contact = db::contacts::get_contact(&pool, id)
        .await
        .map_err(|_| AppError::NotFound)?;
    let book = db::contacts::get_address_book(&pool, contact.address_book_id)
        .await
        .map_err(|_| AppError::NotFound)?;
    if book.owner_user_id != auth.user.id {
        return Err(AppError::Forbidden);
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(&body.data)
        .map_err(|_| AppError::bad_request("data is not valid base64"))?;
    let max = config.attachment_max_bytes as usize;
    if data.len() > max {
        return Err(AppError::bad_request(format!(
            "photo exceeds the {max} byte cap"
        )));
    }
    db::contacts::set_photo(&pool, id, &data, &body.content_type).await?;
    Ok(Json(json!({"ok": true})))
}

// ============ autocomplete ============

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct AutocompleteQuery {
    q: String,
}

/// Attendee typeahead: personal books plus the tenant directory, ranked by
/// name match. Read-only; no CSRF needed.
#[utoipa::path(
    get,
    path = "/api/contacts/autocomplete",
    params(("q" = String, Query, description = "typeahead needle")),
    responses(
        (status = 200, description = "personal contacts plus directory users (max ~10 each)", body = Vec<ContactView>),
    )
)]
async fn autocomplete(
    State(AppState { pool, .. }): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AutocompleteQuery>,
) -> Result<impl IntoResponse, AppError> {
    let auth = resolve_auth(&pool, &headers).await?;
    let tenant_id = db::find_personal_tenant(&pool, auth.user.id).await?;
    let books = db::contacts::list_address_books_for_user(&pool, auth.user.id).await?;
    let book_ids: Vec<Uuid> = books.iter().map(|b| b.id).collect();
    let contacts = db::contacts::search_contacts(&pool, &book_ids, &query.q, 10).await?;
    let mut out = Vec::new();
    for c in contacts {
        let emails = db::contacts::list_emails(&pool, c.id).await?;
        out.push(contact_json(&c, &emails, &[], &[]));
    }
    let needle = query.q.to_lowercase();
    let directory = db::contacts::directory_entries(&pool, tenant_id).await?;
    out.extend(
        directory
            .iter()
            .filter(|e| {
                e.username.to_lowercase().contains(&needle)
                    || e.email.to_lowercase().contains(&needle)
                    || e.display_name
                        .as_deref()
                        .is_some_and(|n| n.to_lowercase().contains(&needle))
            })
            .take(10)
            .map(directory_entry_json),
    );
    Ok(Json(json!(out)))
}

pub fn router() -> axum::Router<crate::AppState> {
    axum::Router::new()
        .route(
            "/api/addressbooks",
            post(create_address_book).get(list_address_books),
        )
        .route(
            "/api/addressbooks/{id}",
            axum::routing::patch(patch_address_book).delete(delete_address_book),
        )
        .route(
            "/api/addressbooks/{id}/contacts",
            post(create_contact).get(list_contacts),
        )
        .route("/api/contacts/autocomplete", get(autocomplete))
        .route(
            "/api/contacts/{id}",
            get(get_contact).patch(patch_contact).delete(delete_contact),
        )
        .route("/api/contacts/{id}/photo", get(get_photo).put(put_photo))
}

/// OpenAPI for the contacts module; merged into the served document in `main.rs`.
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        list_address_books,
        create_address_book,
        patch_address_book,
        delete_address_book,
        list_contacts,
        create_contact,
        get_contact,
        patch_contact,
        delete_contact,
        get_photo,
        put_photo,
        autocomplete,
    ),
    components(schemas(
        AddressBookBody,
        RenameBody,
        AddressBookRowView,
        AddressBookSummary,
        ContactBody,
        EmailBody,
        TelBody,
        ContactView,
        ContactEmailView,
        ContactTelView,
        ContactMemberView,
        PhotoBody,
        AutocompleteQuery,
        crate::OkView,
    ))
)]
pub(crate) struct ContactsApi;
