-- Your SQL goes here
--
Create table if not exists search_items (
  id serial not null primary key,
  track_id INTEGER  NOT NULL,
  track VARCHAR NOT NULL,
  artist VARCHAR NOT NULL,
  album VARCHAR NOT NULL
)

-- #[derive(Debug, Deserialize, Serialize, Clone, Hash, PartialEq, Eq)]
-- pub struct SearchItem {
--     pub id: u64,
--     pub track: String,
--     pub album: String,
--     pub artist: String,
-- }
