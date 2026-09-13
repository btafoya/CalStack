-- Group membership can also point at a tenant user (directory), not only
-- another contact in the same book. A resolved member is one or the other,
-- never both; raw_member (the vCard MEMBER value) is always kept regardless
-- of whether either resolves.
ALTER TABLE contact_group_members ADD COLUMN member_user_id uuid REFERENCES users(id) ON DELETE SET NULL;
ALTER TABLE contact_group_members ADD CONSTRAINT contact_group_members_one_ref
    CHECK (member_contact_id IS NULL OR member_user_id IS NULL);
