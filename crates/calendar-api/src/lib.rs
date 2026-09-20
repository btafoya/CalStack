//! OpenAPI schema generation (docs/PRD.md section 19): the complete public
//! application API as an OpenAPI 3.1 document, served at /api/openapi.json
//! and validated in a unit test. The schema is the API contract; it stays
//! hand-maintained next to the handlers it describes.

use serde_json::json;

fn param(name: &str, path: bool) -> serde_json::Value {
    serde_json::json!({
        "name": name, "in": if path { "path" } else { "query" },
        "required": path, "schema": {"type": "string"},
    })
}

fn json_response(description: &str, schema: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        description: {
            "description": description,
            "content": {"application/json": {"schema": schema}},
        }
    })
}

/// Builds the complete OpenAPI document.
pub fn openapi_document() -> serde_json::Value {
    let user = || serde_json::json!({"$ref": "#/components/schemas/User"});
    let calendar = || serde_json::json!({"$ref": "#/components/schemas/Calendar"});
    let category = || serde_json::json!({"$ref": "#/components/schemas/Category"});
    let contact = || serde_json::json!({"$ref": "#/components/schemas/Contact"});
    let event = || serde_json::json!({"$ref": "#/components/schemas/Event"});

    let mut paths = serde_json::Map::new();
    let mut put = |route: &str, methods: serde_json::Value| {
        paths.insert(route.into(), methods);
    };

    put(
        "/api/auth/register",
        json!({"post": {
            "summary": "Register a local account",
            "requestBody": {"required": true, "content": {"application/json": {"schema": {
                "type": "object", "required": ["username", "email", "password"],
                "properties": {"username": {"type": "string"}, "email": {"type": "string", "format": "email"},
                    "password": {"type": "string", "minLength": 8}, "display_name": {"type": ["string", "null"]}}}}}},
            "responses": {"201": {"description": "created"}, "400": {"description": "validation error"}},
        }}),
    );
    let login_responses = {
        let ok = json_response(
            "session established",
            json!({
            "type": "object",
            "properties": {"csrf_token": {"type": "string"}, "user": user()}}),
        );
        json!({"200": ok["200"], "401": {"description": "unauthorized"}})
    };
    put(
        "/api/auth/login",
        json!({"post": {
            "summary": "Log in (session cookie + CSRF; TOTP code required when 2FA is enabled)",
            "requestBody": {"content": {"application/json": {"schema": {
                "type": "object", "required": ["username_or_email", "password"],
                "properties": {"username_or_email": {"type": "string"}, "password": {"type": "string"},
                    "totp_code": {"type": ["string", "null"]}, "recovery_code": {"type": ["string", "null"]}}}}}},
            "responses": login_responses,
        }}),
    );
    put(
        "/api/auth/logout",
        json!({"post": {
            "summary": "Revoke the current session", "responses": {"200": {"description": "revoked"}}
        }}),
    );
    put(
        "/api/auth/me",
        json!({"get": {
            "summary": "Current user", "responses": json_response("current user", user())
        }}),
    );
    put(
        "/api/auth/password",
        json!({"post": {
            "summary": "Change the current user's password; revokes every other session",
            "requestBody": {"required": true, "content": {"application/json": {"schema": {
                "type": "object", "required": ["current_password", "new_password"],
                "properties": {"current_password": {"type": "string"},
                    "new_password": {"type": "string", "minLength": 8}}}}}},
            "responses": {"200": {"description": "changed"}, "400": {"description": "validation error"},
                "401": {"description": "unauthorized"}},
        }}),
    );

    for (route, summary, secret) in [
        ("/api/auth/tokens", "Scoped API bearer tokens", "secret"),
        (
            "/api/auth/app-passwords",
            "CalDAV Basic-auth app passwords",
            "password",
        ),
    ] {
        let secret_responses = json_response(
            "created; secret shown once",
            json!({
            "type": "object",
            "properties": {"id": {"type": "string", "format": "uuid"}, "secret": {"type": "string"}}}),
        );
        put(
            route,
            json!({
                "post": {"summary": format!("Create {summary}"),
                    "requestBody": {"content": {"application/json": {"schema": {
                        "type": "object", "required": ["name"],
                        "properties": {"name": {"type": "string"}, "scopes": {"type": "array", "items": {"type": "string",
                            "enum": ["read", "write", "full"]},
                            "description": "empty = full access; write implies read; read grants GET only"},
                            "expires_at": {"type": ["string", "null"], "format": "date-time"}}}}}},
                    "responses": secret_responses},
                "get": {"summary": format!("List {summary}"), "responses": {"200": {"description": "list"}}},
            }),
        );
        put(
            &format!("{route}/{{id}}"),
            json!({
                "delete": {"summary": format!("Revoke {secret} credential"),
                    "parameters": [param("id", true)], "responses": {"200": {"description": "revoked"}}}
            }),
        );
    }

    put(
        "/api/auth/totp/setup",
        json!({"post": {
            "summary": "Begin TOTP enrollment",
            "responses": {"201": {"description": "unconfirmed secret + otpauth URL"}}
        }}),
    );
    put(
        "/api/auth/totp/verify",
        json!({"post": {
            "summary": "Confirm TOTP with a first code; returns one-time recovery codes",
            "responses": {"200": {"description": "recovery codes"}, "401": {"description": "bad code"}}
        }}),
    );
    put(
        "/api/auth/totp",
        json!({
            "get": {"summary": "TOTP status", "responses": {"200": {"description": "status"}}},
            "delete": {"summary": "Disable TOTP", "responses": {"200": {"description": "disabled"}}},
        }),
    );

    for route in [
        "/api/auth/webauthn/register/start",
        "/api/auth/webauthn/register/finish",
        "/api/auth/webauthn/login/start",
        "/api/auth/webauthn/login/finish",
    ] {
        put(
            route,
            json!({"post": {
                "summary": format!("Passkey ceremony step: {route}"),
                "requestBody": {"content": {"application/json": {"schema": {"type": "object"}}}},
                "responses": {"200": {"description": "ceremony step"}, "201": {"description": "ceremony step"}}
            }}),
        );
    }
    put(
        "/api/auth/webauthn",
        json!({"get": {
            "summary": "List passkeys", "responses": {"200": {"description": "list"}}
        }}),
    );
    put(
        "/api/auth/webauthn/{id}",
        json!({"delete": {
            "summary": "Remove a passkey", "responses": {"200": {"description": "removed"}}
        }}),
    );

    put(
        "/api/calendars",
        json!({
            "post": {"summary": "Create a calendar in the caller's personal tenant",
                "requestBody": {"content": {"application/json": {"schema": {
                    "type": "object", "required": ["slug", "name"],
                    "properties": {"slug": {"type": "string"}, "name": {"type": "string"},
                        "description": {"type": ["string", "null"]}, "color": {"type": ["string", "null"]},
                        "timezone": {"type": ["string", "null"]}}}}}},
                "responses": json_response("created", calendar())},
            "get": {"summary": "List readable calendars",
                "responses": json_response("list", json!({"type": "array", "items": calendar()}))},
        }),
    );
    put(
        "/api/calendars/{id}",
        json!({
            "get": {"summary": "Get a calendar", "parameters": [param("id", true)],
                "responses": {"200": {"description": "calendar", "content": {"application/json": {"schema": calendar()}}},
                    "404": {"description": "absent"}}},
            "patch": {"summary": "Update calendar properties",
                "requestBody": {"content": {"application/json": {"schema": {
                    "type": "object", "properties": {"name": {"type": ["string", "null"]},
                        "description": {"type": ["string", "null"]}, "color": {"type": ["string", "null"]},
                        "timezone": {"type": ["string", "null"]}, "order_index": {"type": ["integer", "null"]}}}}}},
                "responses": json_response("updated", calendar())},
            "delete": {"summary": "Soft-delete a calendar (retention before purge)",
                "responses": {"200": {"description": "deleted"}}},
        }),
    );
    put(
        "/api/calendars/{id}/acl",
        json!({
            "get": {"summary": "Read the ACL (owner only)", "responses": {"200": {"description": "entries"}}},
            "put": {"summary": "Replace the ACL; requires at least one owner",
                "requestBody": {"content": {"application/json": {"schema": {"type": "object",
                    "properties": {"entries": {"type": "array", "items": {"type": "object"}}}}}}},
                "responses": {"200": {"description": "replaced"}}},
        }),
    );
    put(
        "/api/calendars/{id}/events",
        json!({
            "post": {"summary": "Create an event (master or RECURRENCE-ID exception)",
                "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/EventCreate"}}}},
                "responses": json_response("created", event())},
            "get": {"summary": "List events in a window", "parameters": [param("id", true),
                {"name": "from", "in": "query", "schema": {"type": "string", "format": "date-time"}},
                {"name": "to", "in": "query", "schema": {"type": "string", "format": "date-time"}}],
                "responses": json_response("events", json!({"type": "array", "items": event()}))},
        }),
    );
    put(
        "/api/calendars/{id}/occurrences",
        json!({"get": {
            "summary": "Expanded occurrences with exception overlay",
            "responses": {"200": {"description": "occurrences"}}}
        }),
    );
    put(
        "/api/calendars/{id}/shares",
        json!({
            "post": {"summary": "Create a public share token (owner only)",
                "responses": {"201": {"description": "created; token shown once"}}},
            "get": {"summary": "List live shares", "responses": {"200": {"description": "list"}}},
        }),
    );
    put(
        "/api/calendars/{id}/shares/{share_id}",
        json!({"delete": {
            "summary": "Revoke a share (ADR-004: revocable and expirable)",
            "responses": {"200": {"description": "revoked"}}}
        }),
    );
    put(
        "/api/calendars/{id}/events/{event_id}/attachments",
        json!({
            "post": {"summary": "Upload a capped bytea attachment (base64 body)",
                "requestBody": {"content": {"application/json": {"schema": {"type": "object",
                    "required": ["filename", "content_type", "data"],
                    "properties": {"filename": {"type": "string"}, "content_type": {"type": "string"},
                        "data": {"type": "string", "contentEncoding": "base64"}}}}}},
                "responses": {"201": {"description": "created"}, "400": {"description": "over the size cap"}}},
            "get": {"summary": "List attachment metadata", "responses": {"200": {"description": "list"}}},
        }),
    );
    put(
        "/api/events/{id}",
        json!({
            "get": {"summary": "Get an event with its ETag", "responses": {
                "200": {"description": "event", "content": {"application/json": {"schema": event()}}},
                "404": {"description": "absent"}}},
            "patch": {"summary": "Update with If-Match optimistic concurrency",
                "responses": {"200": {"description": "updated", "content": {"application/json": {"schema": event()}}},
                    "409": {"description": "stale etag"}}},
            "delete": {"summary": "Soft delete (sync-visible tombstone)",
                "responses": {"200": {"description": "deleted"}}},
        }),
    );
    put(
        "/api/attachments/{id}",
        json!({
            "get": {"summary": "Download an attachment", "responses": {"200": {"description": "bytes"}}},
            "delete": {"summary": "Delete an attachment", "responses": {"200": {"description": "deleted"}}},
        }),
    );
    put(
        "/api/attachments/{id}/meta",
        json!({"get": {
            "summary": "Attachment metadata only (no bytes)",
            "responses": {"200": {"description": "metadata"}, "404": {"description": "absent"}},
        }}),
    );
    put(
        "/api/places/autocomplete",
        json!({"get": {
            "summary": "Google Places autocomplete proxy (requires GOOGLE_MAPS_API_KEY)",
            "parameters": [{"name": "q", "in": "query", "required": true, "schema": {"type": "string"}}],
            "responses": {"200": {"description": "suggestions: [{label, place_id}]"}}
        }}),
    );
    put(
        "/api/places/{place_id}",
        json!({"get": {
            "summary": "Resolve a Google place into the structured event location shape",
            "parameters": [param("place_id", true)],
            "responses": {"200": {"description": "location fields"}}
        }}),
    );
    put(
        "/api/search",
        json!({"get": {
            "summary": "Search over summary, description, attendees, locations, categories",
            "parameters": [
                {"name": "q", "in": "query", "schema": {"type": "string"}},
                {"name": "attendee", "in": "query", "schema": {"type": "string"}},
                {"name": "limit", "in": "query", "schema": {"type": "integer"}}],
            "responses": {"200": {"description": "hits"}}
        }}),
    );
    put(
        "/api/changes",
        json!({"get": {
            "summary": "Change stream since a sequence (transport adapters: SSE/WebSocket)",
            "parameters": [{"name": "since", "in": "query", "schema": {"type": "integer"}}],
            "responses": {"200": {"description": "changes"}}
        }}),
    );
    put(
        "/api/notifications",
        json!({"get": {
            "summary": "In-app notifications", "responses": {"200": {"description": "list"}}
        }}),
    );
    put(
        "/api/notifications/{id}/read",
        json!({"post": {
            "summary": "Mark a notification read", "responses": {"200": {"description": "read"}}
        }}),
    );
    put(
        "/api/subscriptions",
        json!({
            "post": {"summary": "Subscribe to a public share",
                "requestBody": {"content": {"application/json": {"schema": {"type": "object",
                    "required": ["share_token"], "properties": {"share_token": {"type": "string"}}}}}},
                "responses": {"201": {"description": "subscribed"}}},
            "get": {"summary": "List subscriptions", "responses": {"200": {"description": "list"}}},
        }),
    );
    put(
        "/api/subscriptions/{id}",
        json!({"delete": {
            "summary": "Unsubscribe", "responses": {"200": {"description": "removed"}}
        }}),
    );
    put(
        "/api/rules",
        json!({
            "post": {"summary": "Create a rule (trigger -> optional conditions -> actions), scoped to one calendar or tenant-wide",
                "requestBody": {"required": true, "content": {"application/json": {"schema": {
                    "type": "object", "required": ["name", "trigger_type"],
                    "properties": {"name": {"type": "string"}, "enabled": {"type": ["boolean", "null"]},
                        "trigger_type": {"type": "string"},
                        "calendar_id": {"type": ["string", "null"], "format": "uuid", "description": "omit/null for tenant-wide (all calendars)"},
                        "conditions": {"type": ["array", "null"]}, "actions": {"type": ["array", "null"]}}}}}},
                "responses": {"201": {"description": "created"}}},
            "get": {"summary": "List rules effective for a calendar (that calendar's rules plus tenant-wide ones)",
                "parameters": [param("calendar_id", false)],
                "responses": {"200": {"description": "list"}}},
        }),
    );
    put(
        "/api/rules/{id}",
        json!({
            "patch": {"summary": "Enable/disable a rule",
                "parameters": [param("id", true)],
                "requestBody": {"required": true, "content": {"application/json": {"schema": {
                    "type": "object", "required": ["enabled"], "properties": {"enabled": {"type": "boolean"}}}}}},
                "responses": {"200": {"description": "updated"}}},
            "delete": {"summary": "Delete a rule", "responses": {"200": {"description": "removed"}}},
        }),
    );
    put(
        "/api/categories",
        json!({
            "post": {"summary": "Create a category registry row (calendar-scoped: owner; tenant-wide: admin)",
                "requestBody": {"required": true, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/CategoryCreate"}}}},
                "responses": {"201": {"description": "created", "content": {"application/json": {"schema": category()}}},
                    "400": {"description": "validation error"}, "409": {"description": "slug already in scope"}}},
            "get": {"summary": "List registry rows: tenant-wide plus rows on calendars visible to the caller",
                "parameters": [param("calendar_id", false)],
                "responses": {"200": {"description": "list", "content": {"application/json": {"schema": {
                    "type": "array", "items": {"$ref": "#/components/schemas/Category"}}}}}}},
        }),
    );
    put(
        "/api/categories/{id}",
        json!({
            "patch": {"summary": "Update a row; a slug change cascades to events in the row's scope",
                "parameters": [param("id", true)],
                "requestBody": {"required": true, "content": {"application/json": {"schema": {
                    "type": "object",
                    "properties": {"slug": {"type": "string"}, "name": {"type": "string"},
                        "color": {"type": "string", "enum": ["blue", "azure", "indigo", "purple", "pink",
                            "red", "orange", "yellow", "lime", "green", "teal", "cyan"]},
                        "sort_order": {"type": "integer"}}}}}},
                "responses": {"200": {"description": "updated"}, "404": {"description": "absent"}}},
            "delete": {"summary": "Delete a row (event category strings are untouched)",
                "responses": {"200": {"description": "removed"}}},
        }),
    );
    put(
        "/api/addressbooks",
        json!({
            "get": {"summary": "List the caller's personal address books plus the read-only tenant directory book",
                "responses": {"200": {"description": "list", "content": {"application/json": {"schema": {
                    "type": "array", "items": {"$ref": "#/components/schemas/AddressBook"}}}}}}},
            "post": {"summary": "Create a personal address book",
                "requestBody": {"required": true, "content": {"application/json": {"schema": {
                    "type": "object", "required": ["slug", "name"],
                    "properties": {"slug": {"type": "string"}, "name": {"type": "string"}}}}}},
                "responses": {"201": {"description": "created"}, "400": {"description": "validation error"}}},
        }),
    );
    put(
        "/api/addressbooks/{id}",
        json!({
            "patch": {"summary": "Rename a personal address book",
                "parameters": [param("id", true)],
                "requestBody": {"required": true, "content": {"application/json": {"schema": {
                    "type": "object", "required": ["name"], "properties": {"name": {"type": "string"}}}}}},
                "responses": {"200": {"description": "updated"}, "403": {"description": "not the owner"}}},
            "delete": {"summary": "Delete a personal address book", "parameters": [param("id", true)],
                "responses": {"200": {"description": "removed"}}},
        }),
    );
    put(
        "/api/addressbooks/{id}/contacts",
        json!({
            "get": {"summary": "List contacts in a book (the directory book id returns projected tenant users)",
                "parameters": [param("id", true)],
                "responses": {"200": {"description": "list", "content": {"application/json": {"schema": {
                    "type": "array", "items": {"$ref": "#/components/schemas/Contact"}}}}}}},
            "post": {"summary": "Create a contact or group (personal books only)",
                "parameters": [param("id", true)],
                "requestBody": {"required": true, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ContactWrite"}}}},
                "responses": {"201": {"description": "created", "content": {"application/json": {"schema": contact()}}}}},
        }),
    );
    put(
        "/api/contacts/autocomplete",
        json!({"get": {
            "summary": "Attendee typeahead: caller's personal books plus the tenant directory, ranked by name/email/phone match",
            "parameters": [{"name": "q", "in": "query", "required": true, "schema": {"type": "string"}}],
            "responses": {"200": {"description": "list", "content": {"application/json": {"schema": {
                "type": "array", "items": {"$ref": "#/components/schemas/Contact"}}}}}},
        }}),
    );
    put(
        "/api/contacts/{id}",
        json!({
            "get": {"summary": "Get a contact", "parameters": [param("id", true)],
                "responses": {"200": {"description": "found", "content": {"application/json": {"schema": contact()}}}, "404": {"description": "absent"}}},
            "patch": {"summary": "Update a contact's normalized fields", "parameters": [param("id", true)],
                "requestBody": {"required": true, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ContactWrite"}}}},
                "responses": {"200": {"description": "updated"}}},
            "delete": {"summary": "Soft-delete a contact", "parameters": [param("id", true)],
                "responses": {"200": {"description": "removed"}}},
        }),
    );
    put(
        "/api/contacts/{id}/photo",
        json!({
            "get": {"summary": "Fetch a contact's photo bytes", "parameters": [param("id", true)],
                "responses": {"200": {"description": "image bytes"}, "404": {"description": "no photo"}}},
            "put": {"summary": "Set a contact's photo (base64, capped like event attachments)",
                "parameters": [param("id", true)],
                "requestBody": {"required": true, "content": {"application/json": {"schema": {
                    "type": "object", "required": ["content_type", "data"],
                    "properties": {"content_type": {"type": "string"}, "data": {"type": "string", "format": "byte"}}}}}},
                "responses": {"200": {"description": "stored"}, "400": {"description": "exceeds the size cap"}}},
        }),
    );
    put(
        "/api/notification-providers",
        json!({
            "post": {"summary": "Configure a notification provider (credentials stored encrypted)",
                "requestBody": {"content": {"application/json": {"schema": {"type": "object",
                    "required": ["kind", "name", "config"],
                    "properties": {"kind": {"type": "string", "enum": ["postmark", "smtp", "twilio", "webpush"]},
                        "name": {"type": "string"}, "config": {"type": "object"}}}}}},
                "responses": {"201": {"description": "configured"}}},
            "get": {"summary": "List providers (no credentials)", "responses": {"200": {"description": "list"}}},
        }),
    );
    put(
        "/api/notification-providers/{id}",
        json!({"delete": {
            "summary": "Remove a provider", "responses": {"200": {"description": "removed"}}
        }}),
    );
    put(
        "/api/webhooks",
        json!({
            "post": {"summary": "Register an outbound webhook (admin only)",
                "requestBody": {"required": true, "content": {"application/json": {"schema": {
                    "type": "object", "required": ["url", "name"],
                    "properties": {"url": {"type": "string", "format": "uri"}, "name": {"type": "string"},
                        "enabled": {"type": ["boolean", "null"]},
                        "sign_key": {"type": ["string", "null"]}}}}}},
                "responses": {"201": {"description": "created"}}},
            "get": {"summary": "List webhooks (admin only)", "responses": {"200": {"description": "list"}}},
        }),
    );
    put(
        "/api/webhooks/{id}",
        json!({
            "patch": {"summary": "Update a webhook (admin only)", "parameters": [param("id", true)],
                "requestBody": {"content": {"application/json": {"schema": {"type": "object",
                    "properties": {"url": {"type": "string"}, "name": {"type": "string"},
                        "enabled": {"type": ["boolean", "null"]},
                        "sign_key": {"type": ["string", "null"]}}}}}},
                "responses": {"200": {"description": "updated"}}},
            "delete": {"summary": "Remove a webhook (admin only)", "parameters": [param("id", true)],
                "responses": {"200": {"description": "removed"}}},
        }),
    );
    put(
        "/api/webhooks/{id}/test",
        json!({"post": {"summary": "Send a signed test delivery (admin only)", "parameters": [param("id", true)],
            "responses": {"200": {"description": "delivery outcome"}}}}),
    );
    put(
        "/api/webhooks/{id}/deliveries",
        json!({"get": {"summary": "Recent deliveries (admin only)", "parameters": [param("id", true)],
            "responses": {"200": {"description": "list"}}}}),
    );
    put(
        "/api/audit",
        json!({"get": {
            "summary": "Audit trail (admin only)", "responses": {"200": {"description": "entries"}}
        }}),
    );
    put(
        "/api/admin/users",
        json!({
            "get": {"summary": "List all users (admin only)", "responses": {"200": {"description": "list"}}},
            "post": {"summary": "Create a user (admin only)",
                "requestBody": {"required": true, "content": {"application/json": {"schema": {
                    "type": "object", "required": ["username", "email", "password"],
                    "properties": {"username": {"type": "string"}, "email": {"type": "string", "format": "email"},
                        "password": {"type": "string", "minLength": 8}, "display_name": {"type": ["string", "null"]},
                        "is_admin": {"type": ["boolean", "null"]}}}}}},
                "responses": {"201": {"description": "created"}, "400": {"description": "validation error"}}},
        }),
    );
    put(
        "/api/admin/users/{id}",
        json!({"patch": {
            "summary": "Promote/demote or enable/disable a user (admin only)",
            "parameters": [param("id", true)],
            "requestBody": {"content": {"application/json": {"schema": {
                "type": "object",
                "properties": {"is_admin": {"type": ["boolean", "null"]}, "disabled": {"type": ["boolean", "null"]}}}}}},
            "responses": {"200": {"description": "updated"}, "400": {"description": "cannot modify own account"}}
        }}),
    );
    put(
        "/webhooks/postmark/inbound",
        json!({"post": {
            "summary": "Inbound iMIP replies (Postmark inbound stream; ADR-009)",
            "responses": {"200": {"description": "processed"}}
        }}),
    );
    put(
        "/share/{token}/calendar.ics",
        json!({"get": {
            "summary": "Anonymous public feed (revocable, optionally expiring)",
            "responses": {"200": {"description": "text/calendar"}, "404": {"description": "no such share"}}
        }}),
    );

    serde_json::json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Calendar Server API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Normalized calendar domain API; CalDAV is a separate first-class protocol.",
        },
        "paths": serde_json::Value::Object(paths),
        "components": {
            "schemas": {
                "User": {"type": "object", "properties": {
                    "id": {"type": "string", "format": "uuid"}, "username": {"type": "string"},
                    "email": {"type": "string"}, "display_name": {"type": ["string", "null"]},
                    "is_admin": {"type": "boolean"},
                }},
                "Event": {"type": "object", "properties": {
                    "id": {"type": "string", "format": "uuid"}, "uid": {"type": "string"},
                    "summary": {"type": "string"}, "etag": {"type": "string"},
                    "sequence": {"type": "integer"},
                    "starts_at": {"type": ["string", "null"], "format": "date-time"},
                    "start_date": {"type": ["string", "null"], "format": "date"},
                    "all_day": {"type": "boolean"},
                    "location": {"$ref": "#/components/schemas/Location"},
                    "categories": {"type": "array", "items": {"type": "string"}},
                    "category_details": {"type": "array", "items": {"$ref": "#/components/schemas/CategoryDetail"}},
                    "attendees": {"type": "array", "items": {"type": "object", "properties": {
                        "email": {"type": "string"}, "display_name": {"type": ["string", "null"]},
                        "role": {"type": "string"}, "partstat": {"type": "string"},
                        "rsvp": {"type": ["boolean", "null"]},
                        "contact_id": {"type": ["string", "null"], "format": "uuid",
                            "description": "loose ref; the contact may since have changed or been deleted"},
                        "user_id": {"type": ["string", "null"], "format": "uuid",
                            "description": "set when the attendee was picked from the tenant directory"},
                    }}},
                }},
                "Category": {"type": "object", "properties": {
                    "id": {"type": "string", "format": "uuid"},
                    "tenant_id": {"type": "string", "format": "uuid"},
                    "calendar_id": {"type": ["string", "null"], "format": "uuid",
                        "description": "null = tenant-wide"},
                    "slug": {"type": "string"}, "name": {"type": "string"},
                    "color": {"type": "string"}, "sort_order": {"type": "integer"},
                }},
                "CategoryCreate": {"type": "object", "required": ["slug", "name", "color"],
                    "properties": {"calendar_id": {"type": ["string", "null"], "format": "uuid",
                        "description": "omit/null for tenant-wide (admin only)"},
                        "slug": {"type": "string"}, "name": {"type": "string"},
                        "color": {"type": "string", "enum": ["blue", "azure", "indigo", "purple", "pink",
                            "red", "orange", "yellow", "lime", "green", "teal", "cyan"]},
                        "sort_order": {"type": "integer"}}},
                "CategoryDetail": {"type": "object", "properties": {
                    "slug": {"type": "string"}, "name": {"type": "string"}, "color": {"type": "string"},
                }},
                "Location": {"type": ["object", "null"], "properties": {
                    "id": {"type": "string", "format": "uuid"},
                    "provider": {"type": ["string", "null"]},
                    "display_name": {"type": ["string", "null"]},
                    "formatted_address": {"type": ["string", "null"]},
                    "latitude": {"type": ["number", "null"]}, "longitude": {"type": ["number", "null"]},
                    "website": {"type": ["string", "null"]}, "phone": {"type": ["string", "null"]},
                }},
                "AddressBook": {"type": "object", "properties": {
                    "id": {"type": "string", "format": "uuid"}, "slug": {"type": "string"},
                    "name": {"type": "string"},
                    "kind": {"type": "string", "enum": ["personal", "directory"],
                        "description": "directory is the read-only, auto-provisioned tenant book"},
                    "ctag": {"type": "integer"},
                }},
                "Contact": {"type": "object", "properties": {
                    "id": {"type": "string", "format": "uuid"},
                    "address_book_id": {"type": ["string", "null"], "format": "uuid"},
                    "uid": {"type": "string"},
                    "kind": {"type": "string", "enum": ["individual", "group"]},
                    "full_name": {"type": "string"},
                    "given_name": {"type": ["string", "null"]}, "family_name": {"type": ["string", "null"]},
                    "org": {"type": ["string", "null"]}, "title": {"type": ["string", "null"]},
                    "has_photo": {"type": "boolean"},
                    "directory": {"type": "boolean",
                        "description": "true for a projected tenant-directory entry (read-only)"},
                    "emails": {"type": "array", "items": {"type": "object", "properties": {
                        "email": {"type": "string"}, "kind": {"type": ["string", "null"]}, "is_primary": {"type": "boolean"}}}},
                    "tels": {"type": "array", "items": {"type": "object", "properties": {
                        "number": {"type": "string"}, "kind": {"type": ["string", "null"]},
                        "is_mobile": {"type": "boolean",
                            "description": "eligible for the planned Twilio SMS channel"},
                        "is_primary": {"type": "boolean"}}}},
                    "members": {"type": "array",
                        "description": "resolved group membership (kind=group only); empty for individuals",
                        "items": {"type": "object", "properties": {
                            "contact_id": {"type": ["string", "null"], "format": "uuid"},
                            "user_id": {"type": ["string", "null"], "format": "uuid",
                                "description": "set when the member is a tenant directory user rather than a contact"},
                            "full_name": {"type": "string"}}}},
                }},
                "ContactWrite": {"type": "object", "required": ["full_name"], "properties": {
                    "kind": {"type": "string", "enum": ["individual", "group"], "default": "individual",
                        "description": "ignored on PATCH — a contact's kind doesn't change after creation"},
                    "full_name": {"type": "string"},
                    "given_name": {"type": ["string", "null"]}, "family_name": {"type": ["string", "null"]},
                    "org": {"type": ["string", "null"]}, "title": {"type": ["string", "null"]},
                    "emails": {"type": "array", "items": {"type": "object", "required": ["email"], "properties": {
                        "email": {"type": "string"}, "kind": {"type": ["string", "null"]}, "is_primary": {"type": "boolean"}}}},
                    "tels": {"type": "array", "items": {"type": "object", "required": ["number"], "properties": {
                        "number": {"type": "string"}, "kind": {"type": ["string", "null"]},
                        "is_mobile": {"type": "boolean"}, "is_primary": {"type": "boolean"}}}},
                    "member_contact_ids": {"type": "array", "items": {"type": "string", "format": "uuid"},
                        "description": "kind=group only: other contacts in the same book; replaces the whole set"},
                    "member_user_ids": {"type": "array", "items": {"type": "string", "format": "uuid"},
                        "description": "kind=group only: tenant directory users; replaces the whole set"},
                }},
                "Calendar": {"type": "object", "properties": {
                    "id": {"type": "string", "format": "uuid"}, "slug": {"type": "string"},
                    "name": {"type": "string"}, "my_capability": {"type": "string",
                        "enum": ["owner", "read_write", "read_only", "free_busy"]},
                }},
            },
            "securitySchemes": {
                "sessionCookie": {"type": "apiKey", "in": "cookie", "name": "session"},
                "bearerToken": {"type": "http", "scheme": "bearer"},
            }
        },
        "security": [{"sessionCookie": []}, {"bearerToken": []}],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_is_valid_with_required_top_level() {
        let doc = openapi_document();
        assert_eq!(doc["openapi"], "3.1.0");
        assert!(doc["paths"].is_object());
        assert!(doc["components"]["schemas"].is_object());
        assert!(doc["paths"]["/api/calendars/{id}/events"]["post"].is_object());
        assert!(doc["paths"]["/api/addressbooks"]["post"].is_object());
        assert!(doc["paths"]["/api/contacts/autocomplete"]["get"].is_object());
        assert!(doc["components"]["schemas"]["Contact"].is_object());
        assert!(doc["components"]["schemas"]["Contact"]["properties"]["members"].is_object());
        assert!(
            doc["components"]["schemas"]["ContactWrite"]["properties"]["member_contact_ids"]
                .is_object()
        );
    }
}
