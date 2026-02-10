-- Your SQL goes here
-- #[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
-- pub struct DownloadableFile {
--     pub filename: String,
--     pub username: String,
--     pub size: u64,
-- }
CREATE TABLE if not exists downloadable_files (
    id serial not null primary key,
    filename VARCHAR NOT NULL,
    username VARCHAR NOT NULL,
    size integer NOT NULL
);
