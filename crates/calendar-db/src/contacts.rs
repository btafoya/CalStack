//! CardDAV contacts (docs/CARDDAV_DESIGN.md): personal address books are real
//! rows here; the tenant directory address book is virtual (projected from
//! tenant_members+users by callers) and never stored. Contacts are canonical
//! normalized rows with the last-written vCard kept verbatim for wire
//! fidelity — the same posture as events (PRD data rules).

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::DbError;

// ============ address books ============

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AddressBookRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub owner_user_id: Uuid,
    pub slug: String,
    pub name: String,
    pub ctag: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Deterministic id for the virtual tenant directory book: not stored, so it
/// must be stable across requests without a row to key off. (ponytail: a
/// sha256-derived UUID rather than pulling in the uuid crate's v5 feature.)
pub fn directory_book_id(tenant_id: Uuid) -> Uuid {
    let digest = calendar_auth::sha256(format!("directory-book-{tenant_id}").as_bytes());
    Uuid::from_slice(&digest[..16]).expect("sha256 digest is at least 16 bytes")
}

pub const DIRECTORY_SLUG: &str = "directory";

pub async fn create_address_book(
    pool: &PgPool,
    tenant_id: Uuid,
    owner_user_id: Uuid,
    slug: &str,
    name: &str,
) -> Result<AddressBookRow, DbError> {
    sqlx::query_as::<_, AddressBookRow>(
        "INSERT INTO address_books (id, tenant_id, owner_user_id, slug, name)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id, tenant_id, owner_user_id, slug, name, ctag, created_at, updated_at",
    )
    .bind(Uuid::new_v4())
    .bind(tenant_id)
    .bind(owner_user_id)
    .bind(slug)
    .bind(name)
    .fetch_one(pool)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            DbError::Conflict(format!("address book slug '{slug}' already exists"))
        }
        _ => DbError::Sql(e),
    })
}

pub async fn list_address_books_for_user(
    pool: &PgPool,
    owner_user_id: Uuid,
) -> Result<Vec<AddressBookRow>, DbError> {
    sqlx::query_as::<_, AddressBookRow>(
        "SELECT id, tenant_id, owner_user_id, slug, name, ctag, created_at, updated_at
         FROM address_books WHERE owner_user_id = $1 AND deleted_at IS NULL ORDER BY name",
    )
    .bind(owner_user_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

/// The default personal book, lazily created on first access so no
/// registration-flow change is needed to provision one.
pub async fn ensure_default_address_book(
    pool: &PgPool,
    tenant_id: Uuid,
    owner_user_id: Uuid,
) -> Result<AddressBookRow, DbError> {
    let existing = list_address_books_for_user(pool, owner_user_id).await?;
    if let Some(book) = existing.into_iter().next() {
        return Ok(book);
    }
    create_address_book(pool, tenant_id, owner_user_id, "contacts", "Contacts").await
}

pub async fn get_address_book_by_slug(
    pool: &PgPool,
    owner_user_id: Uuid,
    slug: &str,
) -> Result<AddressBookRow, DbError> {
    sqlx::query_as::<_, AddressBookRow>(
        "SELECT id, tenant_id, owner_user_id, slug, name, ctag, created_at, updated_at
         FROM address_books WHERE owner_user_id = $1 AND slug = $2 AND deleted_at IS NULL",
    )
    .bind(owner_user_id)
    .bind(slug)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)
}

pub async fn get_address_book(pool: &PgPool, id: Uuid) -> Result<AddressBookRow, DbError> {
    sqlx::query_as::<_, AddressBookRow>(
        "SELECT id, tenant_id, owner_user_id, slug, name, ctag, created_at, updated_at
         FROM address_books WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)
}

pub async fn rename_address_book(
    pool: &PgPool,
    id: Uuid,
    name: &str,
) -> Result<AddressBookRow, DbError> {
    sqlx::query("UPDATE address_books SET name = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(name)
        .execute(pool)
        .await?;
    get_address_book(pool, id).await
}

pub async fn soft_delete_address_book(pool: &PgPool, id: Uuid) -> Result<(), DbError> {
    let n = sqlx::query(
        "UPDATE address_books SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

async fn bump_ctag(tx: &mut sqlx::PgConnection, address_book_id: Uuid) -> Result<(), DbError> {
    sqlx::query("UPDATE address_books SET ctag = ctag + 1, updated_at = now() WHERE id = $1")
        .bind(address_book_id)
        .execute(tx)
        .await?;
    Ok(())
}

// ============ contacts ============

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ContactRow {
    pub id: Uuid,
    pub address_book_id: Uuid,
    pub uid: String,
    pub kind: String,
    pub full_name: String,
    pub given_name: Option<String>,
    pub family_name: Option<String>,
    pub org: Option<String>,
    pub title: Option<String>,
    pub street_address: Option<String>,
    pub locality: Option<String>,
    pub region: Option<String>,
    pub postal_code: Option<String>,
    pub country: Option<String>,
    pub photo_mime: Option<String>,
    pub raw_vcard: String,
    pub etag: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

const CONTACT_COLUMNS: &str = "id, address_book_id, uid, kind, full_name, given_name, family_name,
     org, title, street_address, locality, region, postal_code, country,
     photo_mime, raw_vcard, etag, created_at, updated_at, deleted_at";

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ContactEmailRow {
    pub id: Uuid,
    pub contact_id: Uuid,
    pub email: String,
    pub kind: Option<String>,
    pub is_primary: bool,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ContactTelRow {
    pub id: Uuid,
    pub contact_id: Uuid,
    pub number: String,
    pub kind: Option<String>,
    pub is_mobile: bool,
    pub is_primary: bool,
}

/// A resolved group member, for API display: whichever of contact/user it
/// pointed at, plus that member's display name.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ResolvedMember {
    pub contact_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub full_name: String,
}

#[derive(Debug, Default, Clone)]
pub struct NewEmail {
    pub email: String,
    pub kind: Option<String>,
    pub is_primary: bool,
}

#[derive(Debug, Default, Clone)]
pub struct NewTel {
    pub number: String,
    pub kind: Option<String>,
    pub is_mobile: bool,
    pub is_primary: bool,
}

/// Normalized fields for a contact write, plus the verbatim vCard text the
/// caller already parsed them from (CardDAV PUT body, or generated 3.0 text
/// for API/UI-created contacts).
#[derive(Debug, Default, Clone)]
pub struct NewContact {
    pub uid: String,
    pub kind: String, // "individual" | "group"
    pub full_name: String,
    pub given_name: Option<String>,
    pub family_name: Option<String>,
    pub org: Option<String>,
    pub title: Option<String>,
    pub street_address: Option<String>,
    pub locality: Option<String>,
    pub region: Option<String>,
    pub postal_code: Option<String>,
    pub country: Option<String>,
    pub raw_vcard: String,
    pub emails: Vec<NewEmail>,
    pub tels: Vec<NewTel>,
    /// Raw MEMBER URI values (group contacts only); resolution against
    /// in-book UIDs happens inside upsert_contact.
    pub group_members: Vec<String>,
}

fn contact_etag(contact_id: Uuid, raw_vcard: &str) -> String {
    let digest = calendar_auth::sha256(format!("{contact_id}-{raw_vcard}").as_bytes());
    format!("\"{}\"", calendar_auth::hex_encode(&digest[..8]))
}

/// Insert-or-replace by (address_book_id, uid): CardDAV PUT overwrites a
/// resource wholesale, same as the CalDAV WriteFile path.
pub async fn upsert_contact(
    pool: &PgPool,
    address_book_id: Uuid,
    data: &NewContact,
) -> Result<ContactRow, DbError> {
    let mut tx = pool.begin().await?;
    let existing_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM contacts WHERE address_book_id = $1 AND uid = $2 AND deleted_at IS NULL",
    )
    .bind(address_book_id)
    .bind(&data.uid)
    .fetch_optional(&mut *tx)
    .await?;
    let id = existing_id.unwrap_or_else(Uuid::new_v4);
    let etag = contact_etag(id, &data.raw_vcard);
    sqlx::query(
        "INSERT INTO contacts (id, address_book_id, uid, kind, full_name, given_name, family_name,
            org, title, street_address, locality, region, postal_code, country, raw_vcard, etag)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)
         ON CONFLICT (address_book_id, uid) DO UPDATE SET
            kind = EXCLUDED.kind, full_name = EXCLUDED.full_name,
            given_name = EXCLUDED.given_name, family_name = EXCLUDED.family_name,
            org = EXCLUDED.org, title = EXCLUDED.title,
            street_address = EXCLUDED.street_address, locality = EXCLUDED.locality,
            region = EXCLUDED.region, postal_code = EXCLUDED.postal_code, country = EXCLUDED.country,
            raw_vcard = EXCLUDED.raw_vcard, etag = EXCLUDED.etag, deleted_at = NULL, updated_at = now()",
    )
    .bind(id)
    .bind(address_book_id)
    .bind(&data.uid)
    .bind(&data.kind)
    .bind(&data.full_name)
    .bind(&data.given_name)
    .bind(&data.family_name)
    .bind(&data.org)
    .bind(&data.title)
    .bind(&data.street_address)
    .bind(&data.locality)
    .bind(&data.region)
    .bind(&data.postal_code)
    .bind(&data.country)
    .bind(&data.raw_vcard)
    .bind(&etag)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM contact_emails WHERE contact_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for e in &data.emails {
        sqlx::query(
            "INSERT INTO contact_emails (id, contact_id, email, kind, is_primary) VALUES ($1,$2,$3,$4,$5)",
        )
        .bind(Uuid::new_v4())
        .bind(id)
        .bind(&e.email)
        .bind(&e.kind)
        .bind(e.is_primary)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM contact_tels WHERE contact_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for t in &data.tels {
        sqlx::query(
            "INSERT INTO contact_tels (id, contact_id, number, kind, is_mobile, is_primary)
             VALUES ($1,$2,$3,$4,$5,$6)",
        )
        .bind(Uuid::new_v4())
        .bind(id)
        .bind(&t.number)
        .bind(&t.kind)
        .bind(t.is_mobile)
        .bind(t.is_primary)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM contact_group_members WHERE group_contact_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if !data.group_members.is_empty() {
        let tenant_id: Uuid =
            sqlx::query_scalar("SELECT tenant_id FROM address_books WHERE id = $1")
                .bind(address_book_id)
                .fetch_one(&mut *tx)
                .await?;
        for raw_member in &data.group_members {
            // A member URI referencing another card's UID in this book, or a
            // tenant user's id, resolves to that contact/user; anything else
            // round-trips as raw_member only. Real clients send either an
            // href-style path ("…/{uid}.vcf", Thunderbird) or a bare
            // "urn:uuid:{uid}" (Apple Contacts/DAVx5) — this server emits the
            // latter for both contact- and directory-sourced members.
            let member_uid = raw_member
                .rsplit_once('/')
                .map(|(_, tail)| tail.trim_end_matches(".vcf"))
                .unwrap_or(raw_member.as_str())
                .trim_start_matches("urn:uuid:");
            let member_contact_id: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM contacts WHERE address_book_id = $1 AND uid = $2 AND deleted_at IS NULL",
            )
            .bind(address_book_id)
            .bind(member_uid)
            .fetch_optional(&mut *tx)
            .await?;
            let member_user_id: Option<Uuid> = if member_contact_id.is_some() {
                None
            } else {
                sqlx::query_scalar(
                    "SELECT u.id FROM tenant_members tm JOIN users u ON u.id = tm.user_id
                     WHERE tm.tenant_id = $1 AND u.id::text = $2",
                )
                .bind(tenant_id)
                .bind(member_uid)
                .fetch_optional(&mut *tx)
                .await?
            };
            sqlx::query(
                "INSERT INTO contact_group_members (group_contact_id, raw_member, member_contact_id, member_user_id)
                 VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING",
            )
            .bind(id)
            .bind(raw_member)
            .bind(member_contact_id)
            .bind(member_user_id)
            .execute(&mut *tx)
            .await?;
        }
    }
    bump_ctag(&mut tx, address_book_id).await?;
    tx.commit().await?;
    get_contact(pool, id).await
}

pub async fn get_contact(pool: &PgPool, id: Uuid) -> Result<ContactRow, DbError> {
    sqlx::query_as::<_, ContactRow>(&format!(
        "SELECT {CONTACT_COLUMNS} FROM contacts WHERE id = $1 AND deleted_at IS NULL"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)
}

pub async fn get_contact_by_uid(
    pool: &PgPool,
    address_book_id: Uuid,
    uid: &str,
) -> Result<ContactRow, DbError> {
    sqlx::query_as::<_, ContactRow>(&format!(
        "SELECT {CONTACT_COLUMNS} FROM contacts
         WHERE address_book_id = $1 AND uid = $2 AND deleted_at IS NULL"
    ))
    .bind(address_book_id)
    .bind(uid)
    .fetch_optional(pool)
    .await?
    .ok_or(DbError::NotFound)
}

pub async fn list_contacts(
    pool: &PgPool,
    address_book_id: Uuid,
) -> Result<Vec<ContactRow>, DbError> {
    sqlx::query_as::<_, ContactRow>(&format!(
        "SELECT {CONTACT_COLUMNS} FROM contacts
         WHERE address_book_id = $1 AND deleted_at IS NULL ORDER BY full_name"
    ))
    .bind(address_book_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

/// All soft-deleted contacts with a deletion after `since`, for sync-collection.
pub async fn list_deleted_contacts(
    pool: &PgPool,
    address_book_id: Uuid,
) -> Result<Vec<Uuid>, DbError> {
    sqlx::query_scalar(
        "SELECT id FROM contacts WHERE address_book_id = $1 AND deleted_at IS NOT NULL",
    )
    .bind(address_book_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

pub async fn soft_delete_contact(pool: &PgPool, id: Uuid) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let address_book_id: Option<Uuid> = sqlx::query_scalar(
        "UPDATE contacts SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL
         RETURNING address_book_id",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(address_book_id) = address_book_id else {
        return Err(DbError::NotFound);
    };
    bump_ctag(&mut tx, address_book_id).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn list_emails(pool: &PgPool, contact_id: Uuid) -> Result<Vec<ContactEmailRow>, DbError> {
    sqlx::query_as::<_, ContactEmailRow>(
        "SELECT id, contact_id, email, kind, is_primary FROM contact_emails
         WHERE contact_id = $1 ORDER BY is_primary DESC",
    )
    .bind(contact_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

pub async fn list_tels(pool: &PgPool, contact_id: Uuid) -> Result<Vec<ContactTelRow>, DbError> {
    sqlx::query_as::<_, ContactTelRow>(
        "SELECT id, contact_id, number, kind, is_mobile, is_primary FROM contact_tels
         WHERE contact_id = $1 ORDER BY is_primary DESC",
    )
    .bind(contact_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

/// Group members with display names resolved, for the API response. Members
/// whose target has since been deleted are silently dropped (their
/// raw_member row still exists for CardDAV round-trip, but there's nothing
/// to display).
pub async fn resolved_group_members(
    pool: &PgPool,
    group_contact_id: Uuid,
) -> Result<Vec<ResolvedMember>, DbError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        contact_id: Option<Uuid>,
        user_id: Option<Uuid>,
        full_name: String,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT c.id AS contact_id, NULL::uuid AS user_id, c.full_name
         FROM contact_group_members g JOIN contacts c ON c.id = g.member_contact_id
         WHERE g.group_contact_id = $1 AND c.deleted_at IS NULL
         UNION ALL
         SELECT NULL::uuid AS contact_id, u.id AS user_id, COALESCE(u.display_name, u.username)
         FROM contact_group_members g JOIN users u ON u.id = g.member_user_id
         WHERE g.group_contact_id = $1",
    )
    .bind(group_contact_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ResolvedMember {
            contact_id: r.contact_id,
            user_id: r.user_id,
            full_name: r.full_name,
        })
        .collect())
}

pub async fn get_photo(pool: &PgPool, contact_id: Uuid) -> Result<(Vec<u8>, String), DbError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        photo: Option<Vec<u8>>,
        photo_mime: Option<String>,
    }
    let row: Row = sqlx::query_as("SELECT photo, photo_mime FROM contacts WHERE id = $1")
        .bind(contact_id)
        .fetch_optional(pool)
        .await?
        .ok_or(DbError::NotFound)?;
    match row.photo {
        Some(data) => Ok((data, row.photo_mime.unwrap_or_else(|| "image/jpeg".into()))),
        None => Err(DbError::NotFound),
    }
}

/// The size cap is enforced by the caller (env-configurable, mirrors
/// attachments/ADR-010); this just stores what it is given.
pub async fn set_photo(
    pool: &PgPool,
    contact_id: Uuid,
    data: &[u8],
    mime: &str,
) -> Result<(), DbError> {
    let n = sqlx::query("UPDATE contacts SET photo = $2, photo_mime = $3, updated_at = now() WHERE id = $1 AND deleted_at IS NULL")
        .bind(contact_id)
        .bind(data)
        .bind(mime)
        .execute(pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Full-text-ish search over name/org plus exact-ish email/phone match, for
/// the attendee autocomplete endpoint. address_book_ids scopes to books the
/// caller can see (their own personal books; directory handled separately by
/// the caller since it is virtual).
pub async fn search_contacts(
    pool: &PgPool,
    address_book_ids: &[Uuid],
    query: &str,
    limit: i64,
) -> Result<Vec<ContactRow>, DbError> {
    if address_book_ids.is_empty() {
        return Ok(vec![]);
    }
    let like = format!("%{}%", query.replace(['%', '_'], ""));
    sqlx::query_as::<_, ContactRow>(&format!(
        "SELECT DISTINCT {cols} FROM contacts c
         LEFT JOIN contact_emails ce ON ce.contact_id = c.id
         LEFT JOIN contact_tels ct ON ct.contact_id = c.id
         WHERE c.address_book_id = ANY($1) AND c.deleted_at IS NULL
           AND (c.full_name ILIKE $2 OR c.org ILIKE $2 OR ce.email ILIKE $2 OR ct.number ILIKE $2)
         ORDER BY c.full_name LIMIT $3",
        cols = CONTACT_COLUMNS
            .split(", ")
            .map(|c| format!("c.{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
    .bind(address_book_ids)
    .bind(&like)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

// ============ virtual tenant directory ============

/// A tenant user projected as a directory "contact" — never stored; rebuilt
/// on every read from tenant_members+users.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DirectoryEntry {
    pub user_id: Uuid,
    pub username: String,
    pub email: String,
    pub display_name: Option<String>,
    pub updated_at: DateTime<Utc>,
}

pub async fn directory_entries(
    pool: &PgPool,
    tenant_id: Uuid,
) -> Result<Vec<DirectoryEntry>, DbError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        user_id: Uuid,
        username: String,
        email: String,
        display_name: Option<String>,
        updated_at: DateTime<Utc>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT u.id as user_id, u.username, u.email, u.display_name, u.updated_at
         FROM tenant_members tm JOIN users u ON u.id = tm.user_id
         WHERE tm.tenant_id = $1 ORDER BY u.username",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| DirectoryEntry {
            user_id: r.user_id,
            username: r.username,
            email: r.email,
            display_name: r.display_name,
            updated_at: r.updated_at,
        })
        .collect())
}

/// Stateless ctag for the virtual directory book: changes whenever tenant
/// membership or a member's profile changes.
pub async fn directory_ctag(pool: &PgPool, tenant_id: Uuid) -> Result<String, DbError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        count: i64,
        max_updated: Option<DateTime<Utc>>,
    }
    let row: Row = sqlx::query_as(
        "SELECT COUNT(*) as count, MAX(u.updated_at) as max_updated
         FROM tenant_members tm JOIN users u ON u.id = tm.user_id WHERE tm.tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await?;
    Ok(format!(
        "{}-{}",
        row.count,
        row.max_updated.map(|t| t.timestamp()).unwrap_or(0)
    ))
}
