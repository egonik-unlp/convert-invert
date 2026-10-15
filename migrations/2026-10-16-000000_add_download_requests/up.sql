-- "Download what's missing from this collection", asked by one person. The run that serves it
-- has its own queue; its tracks point back here so the shared queue can say who asked.
CREATE TABLE IF NOT EXISTS download_requests (
    id SERIAL PRIMARY KEY,
    user_id INTEGER REFERENCES users (id) ON DELETE SET NULL,
    source_kind VARCHAR NOT NULL,
    source_id VARCHAR NOT NULL,
    source_name VARCHAR NOT NULL,
    options JSONB NOT NULL DEFAULT '{}'::jsonb,
    track_count INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS download_requests_user_id_idx ON download_requests (user_id);

-- Latest request that asked for the track (a track is one row, shared by every playlist).
ALTER TABLE search_items
    ADD COLUMN IF NOT EXISTS request_id INTEGER REFERENCES download_requests (id) ON DELETE SET NULL;

-- Someone chose a rejected candidate by hand ("use this file anyway").
CREATE TABLE IF NOT EXISTS manual_accepts (
    id SERIAL PRIMARY KEY,
    track INTEGER NOT NULL REFERENCES search_items (id) ON DELETE CASCADE,
    downloadable_file INTEGER NOT NULL REFERENCES downloadable_files (id) ON DELETE CASCADE,
    user_id INTEGER REFERENCES users (id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
