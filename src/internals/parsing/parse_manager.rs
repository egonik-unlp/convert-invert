pub struct ParseManager {
    query_playlist: String,
}
impl ParseManager {
    pub fn new(query_playlist: impl Into<String>) -> Self {
        let query_playlist = query_playlist.into();
        ParseManager { query_playlist }
    }
}
