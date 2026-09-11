# Implementation Checklist

Build in this order:

1. Workspace/tooling/configuration.
2. Complete PostgreSQL schema and migration framework.
3. Domain types and validation.
4. Authentication: passwords, sessions, API tokens, app passwords, TOTP, WebAuthn.
5. Calendar CRUD and ACL engine.
6. Event CRUD and optimistic concurrency.
7. iCalendar parser/serializer and recurrence engine.
8. dav-server-rs PostgreSQL adapter.
9. CalDAV discovery, calendar-query, multiget, sync-token and resource operations.
10. Scheduling/iTIP/iMIP.
11. VALARM and PostgreSQL durable scheduler.
12. Attachments and streaming.
13. Search.
14. Public shares and subscriptions.
15. OpenAPI generation and complete CRUD/operations.
16. Rules and notification providers.
17. Web UI and embedded assets.
18. Backup/export/import.
19. Audit and retention/purge.
20. Interoperability suite and performance/security hardening.

At each stage follow CLAUDE.md's implementation loop.
