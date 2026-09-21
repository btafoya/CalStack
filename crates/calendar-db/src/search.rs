//! Search (docs/PRD.md section 12): PostgreSQL-native over summary, plain
//! description, attendees, location fields, categories and metadata.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::journals::JournalRow;
use super::tasks::TaskRow;
use super::{DbError, EventRow};

/// Free-text search across every calendar the user can read (owner,
/// read_write, read_only; free_busy-only access yields nothing). Text hits on
/// the generated tsvector; attendees and locations match by substring.
pub struct SearchQuery {
    pub text: String,
    pub attendee: Option<String>,
    pub limit: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SearchHit {
    pub event: EventRow,
    pub rank: f32,
}

pub async fn search_events(
    pool: &PgPool,
    user_id: Uuid,
    query: &SearchQuery,
) -> Result<Vec<SearchHit>, DbError> {
    let limit = if query.limit <= 0 { 50 } else { query.limit };
    let pattern = format!("%{}%", query.attendee.as_deref().unwrap_or_default());
    #[derive(sqlx::FromRow)]
    struct Raw {
        #[sqlx(flatten)]
        event: EventRow,
        rank: f32,
    }
    let rows = sqlx::query_as::<_, Raw>(
        "SELECT e.*, ts_rank(e.search_vector, websearch_to_tsquery('simple', $2)) AS rank
         FROM events e
         JOIN calendars c ON c.id = e.calendar_id AND c.deleted_at IS NULL
         WHERE c.deleted_at IS NULL AND e.deleted_at IS NULL
           AND EXISTS (
             SELECT 1 FROM calendar_acl acl
             WHERE acl.calendar_id = e.calendar_id AND acl.principal_user_id = $1
               AND acl.capability IN ('owner', 'read_write', 'read_only')
           )
           AND (
             (e.search_vector @@ websearch_to_tsquery('simple', $2) AND $2 <> '')
             OR EXISTS (SELECT 1 FROM event_attendees a
                        WHERE a.event_id = e.id AND (a.email ILIKE $3 OR a.display_name ILIKE $3) AND $4)
             OR EXISTS (SELECT 1 FROM locations l
                        WHERE l.id = e.location_id AND (l.display_name ILIKE $3 OR l.formatted_address ILIKE $3) AND $4)
             OR $5 = ANY(e.categories)
           )
         ORDER BY rank DESC, e.starts_at
         LIMIT $6",
    )
    .bind(user_id)
    .bind(&query.text)
    .bind(&pattern)
    .bind(query.attendee.is_some())
    .bind(query.attendee.clone().unwrap_or_default())
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SearchHit {
            event: r.event,
            rank: r.rank,
        })
        .collect())
}

/// Tasks (ADR-015) hit by the same rules as `search_events`: generated
/// tsvector on summary/description, ACL-scoped (free_busy-only access yields
/// nothing), attendees and the plain `location` column by substring,
/// categories by exact tag. Includes override rows, like events.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TaskSearchHit {
    pub task: TaskRow,
    pub rank: f32,
}

pub async fn search_tasks(
    pool: &PgPool,
    user_id: Uuid,
    query: &SearchQuery,
) -> Result<Vec<TaskSearchHit>, DbError> {
    let limit = if query.limit <= 0 { 50 } else { query.limit };
    let pattern = format!("%{}%", query.attendee.as_deref().unwrap_or_default());
    #[derive(sqlx::FromRow)]
    struct Raw {
        #[sqlx(flatten)]
        task: TaskRow,
        rank: f32,
    }
    let rows = sqlx::query_as::<_, Raw>(
        "SELECT t.*, ts_rank(t.search_vector, websearch_to_tsquery('simple', $2)) AS rank
         FROM tasks t
         JOIN calendars c ON c.id = t.calendar_id AND c.deleted_at IS NULL
         WHERE c.deleted_at IS NULL AND t.deleted_at IS NULL
           AND EXISTS (
             SELECT 1 FROM calendar_acl acl
             WHERE acl.calendar_id = t.calendar_id AND acl.principal_user_id = $1
               AND acl.capability IN ('owner', 'read_write', 'read_only')
           )
           AND (
             (t.search_vector @@ websearch_to_tsquery('simple', $2) AND $2 <> '')
             OR EXISTS (SELECT 1 FROM task_attendees a
                        WHERE a.task_id = t.id AND (a.email ILIKE $3 OR a.display_name ILIKE $3) AND $4)
             OR (t.location ILIKE $3 AND $4)
             OR $5 = ANY(t.categories)
           )
         ORDER BY rank DESC, t.due_at NULLS LAST
         LIMIT $6",
    )
    .bind(user_id)
    .bind(&query.text)
    .bind(&pattern)
    .bind(query.attendee.is_some())
    .bind(query.attendee.clone().unwrap_or_default())
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| TaskSearchHit {
            task: r.task,
            rank: r.rank,
        })
        .collect())
}

/// Journals hit by the same rules; they have no attendees or location, so an
/// `attendee` filter yields nothing (an attendee-scoped search finds nothing
/// among the attendee-less kind rather than ignoring the filter).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JournalSearchHit {
    pub journal: JournalRow,
    pub rank: f32,
}

pub async fn search_journals(
    pool: &PgPool,
    user_id: Uuid,
    query: &SearchQuery,
) -> Result<Vec<JournalSearchHit>, DbError> {
    if query.attendee.is_some() {
        return Ok(Vec::new());
    }
    let limit = if query.limit <= 0 { 50 } else { query.limit };
    #[derive(sqlx::FromRow)]
    struct Raw {
        #[sqlx(flatten)]
        journal: JournalRow,
        rank: f32,
    }
    let rows = sqlx::query_as::<_, Raw>(
        "SELECT j.*, ts_rank(j.search_vector, websearch_to_tsquery('simple', $2)) AS rank
         FROM journals j
         JOIN calendars c ON c.id = j.calendar_id AND c.deleted_at IS NULL
         WHERE c.deleted_at IS NULL AND j.deleted_at IS NULL
           AND EXISTS (
             SELECT 1 FROM calendar_acl acl
             WHERE acl.calendar_id = j.calendar_id AND acl.principal_user_id = $1
               AND acl.capability IN ('owner', 'read_write', 'read_only')
           )
           AND (
             (j.search_vector @@ websearch_to_tsquery('simple', $2) AND $2 <> '')
             OR $3 = ANY(j.categories)
           )
         ORDER BY rank DESC, j.starts_at DESC NULLS LAST
         LIMIT $4",
    )
    .bind(user_id)
    .bind(&query.text)
    .bind(query.attendee.clone().unwrap_or_default())
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| JournalSearchHit {
            journal: r.journal,
            rank: r.rank,
        })
        .collect())
}

/// Recent change entries across readable calendars — the application change
/// stream (docs/PRD.md section 11), surfaced over OpenAPI.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChangeRow {
    pub seq: i64,
    pub calendar_id: Uuid,
    pub resource_id: Uuid,
    pub operation: String,
    pub changed_at: DateTime<Utc>,
}

/// Changes after a sync-token across every calendar the user can read.
pub async fn list_changes_since(
    pool: &PgPool,
    user_id: Uuid,
    since_seq: i64,
    limit: i64,
) -> Result<Vec<ChangeRow>, DbError> {
    sqlx::query_as::<_, ChangeRow>(
        "SELECT cl.seq, cl.calendar_id, cl.resource_id, cl.operation, cl.changed_at
         FROM change_log cl
         JOIN calendar_acl acl ON acl.calendar_id = cl.calendar_id
             AND acl.principal_user_id = $1
             AND acl.capability IN ('owner', 'read_write', 'read_only')
         WHERE cl.seq > $2
         ORDER BY cl.seq
         LIMIT $3",
    )
    .bind(user_id)
    .bind(since_seq)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

#[cfg(test)]
mod search_tests {
    use super::*;
    use uuid::Uuid;

    fn uid(seed: u128) -> Uuid {
        Uuid::from_u128(seed)
    }

    /// Owner calendar with a task and a journal, plus a second calendar holding
    /// a task the user has no ACL row for.
    async fn seed(pool: &PgPool) {
        let sql = format!(
            "INSERT INTO users (id, username, email, display_name, password_hash) \
                 VALUES ('{u}', 'alice', 'alice@example.com', 'Alice', 'argon2id$hash'); \
             INSERT INTO tenants (id, slug, name, is_personal) \
                 VALUES ('{t}', 'alice', 'Alice', true); \
             INSERT INTO calendars (id, tenant_id, slug, name) \
                 VALUES ('{c}', '{t}', 'personal', 'Personal'); \
             INSERT INTO calendar_acl (calendar_id, principal_user_id, capability) \
                 VALUES ('{c}', '{u}', 'owner'); \
             INSERT INTO calendars (id, tenant_id, slug, name) \
                 VALUES ('{c2}', '{t}', 'secret', 'Secret'); \
             INSERT INTO tasks (id, calendar_id, uid, due_at, summary, description_text, \
                 status, percent_complete, parent_uid) \
                 VALUES ('{t1}', '{c}', 'ship-1', '2026-09-21T17:00:00+00:00', \
                     'Ship the release', 'tag the repository and publish', 'IN-PROCESS', 50, \
                     NULL); \
             INSERT INTO tasks (id, calendar_id, uid, summary, parent_uid) \
                 VALUES ('{t2}', '{c}', 'sub-1', 'Tag the repository', 'ship-1'); \
             INSERT INTO task_attendees (id, task_id, email, display_name, partstat) \
                 VALUES ('{a1}', '{t1}', 'bob@example.com', 'Bob', 'ACCEPTED'); \
             INSERT INTO task_alarms (id, task_id, action, related, offset_interval, description) \
                 VALUES ('{al}', '{t1}', 'DISPLAY', 'END', '-00:10:00', 'Ship soon'); \
             INSERT INTO tasks (id, calendar_id, uid, summary) \
                 VALUES ('{t3}', '{c2}', 'hidden-1', 'Hidden deliverable'); \
             INSERT INTO journals (id, calendar_id, uid, summary, description_text, status) \
                 VALUES ('{j1}', '{c}', 'note-1', 'Sprint retro', 'went well overall', 'FINAL'); \
             INSERT INTO journals (id, calendar_id, uid, summary, categories) \
                 VALUES ('{j2}', '{c}', 'note-2', 'Quarterly planning', '{{work}}');",
            u = uid(0x31),
            t = uid(0x32),
            c = uid(0x33),
            c2 = uid(0x34),
            t1 = uid(0x35),
            t2 = uid(0x36),
            t3 = uid(0x37),
            a1 = uid(0x38),
            al = uid(0x39),
            j1 = uid(0x3a),
            j2 = uid(0x3b)
        );
        for statement in sql.split(';').filter(|s| !s.trim().is_empty()) {
            sqlx::query(statement).execute(pool).await.unwrap();
        }
    }

    async fn test_db(base_url: &str, name: &str) -> (sqlx::PgPool, String) {
        let admin = super::super::connect(base_url, 2).await.unwrap();
        let full = format!("calsearch_{name}_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {full}"))
            .execute(&admin)
            .await
            .unwrap();
        let url = format!("{}/{}", base_url.rsplit_once('/').unwrap().0, full);
        let pool = super::super::connect(&url, 4).await.unwrap();
        super::super::migrate(&pool).await.unwrap();
        seed(&pool).await;
        (pool, full)
    }

    async fn drop_db(base_url: &str, name: &str) {
        let admin = super::super::connect(base_url, 2).await.unwrap();
        let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .execute(&admin)
            .await;
    }

    #[tokio::test]
    async fn search_finds_tasks_journals_and_respects_acl() {
        let Some(base_url) = std::env::var("CALENDAR_DB_TEST_URL").ok() else {
            eprintln!("skipping: CALENDAR_DB_TEST_URL is not set");
            return;
        };
        let (pool, name) = test_db(&base_url, "main").await;
        let user = uid(0x31);
        let q = |text: &str| SearchQuery {
            text: text.to_string(),
            attendee: None,
            limit: 50,
        };

        // Text hits the generated tsvector; subtask matches its own summary.
        let tasks = search_tasks(&pool, user, &q("ship release")).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task.uid, "ship-1");
        assert_eq!(
            tasks[0].task.due_at.map(|d| d.to_rfc3339()).unwrap(),
            "2026-09-21T17:00:00+00:00"
        );

        // The subtask matches on its summary; its master matches on the same
        // word in its description — but the ACL-less calendar's task does not.
        let subtasks = search_tasks(&pool, user, &q("repository")).await.unwrap();
        assert!(
            subtasks
                .iter()
                .any(|hit| hit.task.uid == "sub-1"
                    && hit.task.parent_uid.as_deref() == Some("ship-1"))
        );
        assert!(!subtasks.is_empty());

        // The task on the ACL-less calendar is not visible.
        let hidden = search_tasks(&pool, user, &q("hidden")).await.unwrap();
        assert!(hidden.is_empty());

        // Attendee substring matches the task attendee.
        let by_attendee = search_tasks(
            &pool,
            user,
            &SearchQuery {
                text: String::new(),
                attendee: Some("bob".into()),
                limit: 50,
            },
        )
        .await
        .unwrap();
        assert_eq!(by_attendee.len(), 1);
        assert_eq!(by_attendee[0].task.uid, "ship-1");

        // Journals hit on description text and categories.
        let journals = search_journals(&pool, user, &q("went well")).await.unwrap();
        assert_eq!(journals.len(), 1);
        assert_eq!(journals[0].journal.uid, "note-1");
        let by_category = search_journals(&pool, user, &q("quarterly planning"))
            .await
            .unwrap();
        assert_eq!(by_category.len(), 1);
        assert_eq!(by_category[0].journal.categories, vec!["work".to_string()]);

        // An attendee-scoped search yields no journals (no attendee rows).
        let none = search_journals(
            &pool,
            user,
            &SearchQuery {
                text: "planning".into(),
                attendee: Some("bob".into()),
                limit: 50,
            },
        )
        .await
        .unwrap();
        assert!(none.is_empty());

        drop_db(&base_url, &name).await;
    }
}
