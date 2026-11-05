-- The pipeline list shows the variables a run was started with (Jenkins style) instead of
-- the commit title. Turn it off per repository to get the commit title back.
ALTER TABLE repos ADD COLUMN list_shows_variables BOOLEAN NOT NULL DEFAULT TRUE;
