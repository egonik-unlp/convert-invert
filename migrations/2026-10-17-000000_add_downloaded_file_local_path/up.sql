-- Where the file ended up on disk (the download volume's path, e.g. /downloads/My Mix/song.flac).
-- `filename` is the Soulseek remote name, which is not enough to find the file again. NULL for
-- rows written before this column; the API fills those in when it can match them.
ALTER TABLE downloaded_file ADD COLUMN local_path VARCHAR;
