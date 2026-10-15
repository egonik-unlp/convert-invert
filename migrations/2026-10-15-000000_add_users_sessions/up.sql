-- People who can use the dashboard. A row starts as an invitation (email only) and is bound to a
-- Spotify account the first time that person logs in with a matching email.
CREATE TABLE IF NOT EXISTS users (
    id SERIAL PRIMARY KEY,
    spotify_id VARCHAR UNIQUE,
    email VARCHAR NOT NULL UNIQUE,
    display_name VARCHAR,
    avatar_url VARCHAR,
    role VARCHAR NOT NULL DEFAULT 'member' CHECK (role IN ('admin', 'member')),
    status VARCHAR NOT NULL DEFAULT 'invited' CHECK (status IN ('invited', 'active', 'disabled')),
    locale VARCHAR,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ
);

-- Browser sessions. Only the sha256 of the cookie token is stored, so a database leak does not
-- hand out live sessions.
CREATE TABLE IF NOT EXISTS sessions (
    id SERIAL PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    token_hash VARCHAR NOT NULL UNIQUE,
    user_agent VARCHAR,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_user_id_idx ON sessions (user_id);

-- Each user's Spotify tokens, AES-256-GCM encrypted with TOKEN_ENC_KEY (nonce-prefixed).
CREATE TABLE IF NOT EXISTS spotify_tokens (
    user_id INTEGER PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    refresh_token_enc BYTEA NOT NULL,
    access_token_enc BYTEA,
    access_expires_at TIMESTAMPTZ,
    scopes VARCHAR NOT NULL DEFAULT ''
);
