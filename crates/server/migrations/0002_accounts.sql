-- Local accounts and roles.
ALTER TABLE users ADD COLUMN password_hash TEXT;
ALTER TABLE users ADD COLUMN is_admin BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE users ADD COLUMN disabled BOOLEAN NOT NULL DEFAULT FALSE;

-- Existing installations: the oldest account becomes the first admin.
UPDATE users SET is_admin = TRUE WHERE id = (SELECT min(id) FROM users);
