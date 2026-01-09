use serde::{Serialize, Deserialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructTracksItemsItemTrackAlbumExternalUrls {
    pub spotify: Option<String>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructTracksItemsItemVideoThumbnail {
    pub url: Option<serde_json::Value>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructTracksItemsItemTrackExternalUrls {
    pub spotify: Option<String>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructExternalUrls {
    pub spotify: Option<String>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructTracksItemsItem {
    pub added_at: Option<String>,
    pub added_by: Option<RootStructTracksItemsItemAddedBy>,
    pub is_local: Option<bool>,
    pub primary_color: Option<serde_json::Value>,
    pub track: Option<RootStructTracksItemsItemTrack>,
    pub video_thumbnail: Option<RootStructTracksItemsItemVideoThumbnail>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructTracksItemsItemTrackAlbumArtistsItemExternalUrls {
    pub spotify: Option<String>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructFollowers {
    pub href: Option<serde_json::Value>,
    pub total: Option<i64>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructOwner {
    pub display_name: Option<String>,
    pub external_urls: Option<RootStructOwnerExternalUrls>,
    pub href: Option<String>,
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    pub uri: Option<String>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructTracksItemsItemTrackAlbum {
    pub album_type: Option<String>,
    pub artists: Option<Vec<RootStructTracksItemsItemTrackAlbumArtistsItem>>,
    pub available_markets: Option<Vec<String>>,
    pub external_urls: Option<RootStructTracksItemsItemTrackAlbumExternalUrls>,
    pub href: Option<String>,
    pub id: Option<String>,
    pub images: Option<Vec<RootStructTracksItemsItemTrackAlbumImagesItem>>,
    pub name: Option<String>,
    pub release_date: Option<String>,
    pub release_date_precision: Option<String>,
    pub total_tracks: Option<i64>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    pub uri: Option<String>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructImagesItem {
    pub height: Option<i64>,
    pub url: Option<String>,
    pub width: Option<i64>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructTracksItemsItemAddedByExternalUrls {
    pub spotify: Option<String>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructTracksItemsItemTrackAlbumArtistsItem {
    pub external_urls: Option<RootStructTracksItemsItemTrackAlbumArtistsItemExternalUrls>,
    pub href: Option<String>,
    pub id: Option<String>,
    pub name: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    pub uri: Option<String>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructTracksItemsItemTrack {
    pub album: Option<RootStructTracksItemsItemTrackAlbum>,
    pub artists: Option<Vec<RootStructTracksItemsItemTrackArtistsItem>>,
    pub available_markets: Option<Vec<String>>,
    pub disc_number: Option<i64>,
    pub duration_ms: Option<i64>,
    pub episode: Option<bool>,
    pub explicit: Option<bool>,
    pub external_ids: Option<RootStructTracksItemsItemTrackExternalIds>,
    pub external_urls: Option<RootStructTracksItemsItemTrackExternalUrls>,
    pub href: Option<String>,
    pub id: Option<String>,
    pub is_local: Option<bool>,
    pub name: Option<String>,
    pub popularity: Option<i64>,
    pub preview_url: Option<serde_json::Value>,
    pub track: Option<bool>,
    pub track_number: Option<i64>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    pub uri: Option<String>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructOwnerExternalUrls {
    pub spotify: Option<String>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructTracksItemsItemTrackAlbumImagesItem {
    pub height: Option<i64>,
    pub url: Option<String>,
    pub width: Option<i64>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStruct {
    pub collaborative: Option<bool>,
    pub description: Option<String>,
    pub external_urls: Option<RootStructExternalUrls>,
    pub followers: Option<RootStructFollowers>,
    pub href: Option<String>,
    pub id: Option<String>,
    pub images: Option<Vec<RootStructImagesItem>>,
    pub name: Option<String>,
    pub owner: Option<RootStructOwner>,
    pub primary_color: Option<serde_json::Value>,
    pub public: Option<bool>,
    pub snapshot_id: Option<String>,
    pub tracks: Option<RootStructTracks>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    pub uri: Option<String>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructTracksItemsItemAddedBy {
    pub external_urls: Option<RootStructTracksItemsItemAddedByExternalUrls>,
    pub href: Option<String>,
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    pub uri: Option<String>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructTracksItemsItemTrackExternalIds {
    pub isrc: Option<String>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructTracksItemsItemTrackArtistsItem {
    pub external_urls: Option<RootStructTracksItemsItemTrackArtistsItemExternalUrls>,
    pub href: Option<String>,
    pub id: Option<String>,
    pub name: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    pub uri: Option<String>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructTracksItemsItemTrackArtistsItemExternalUrls {
    pub spotify: Option<String>,
}


#[derive(Debug, Serialize, Deserialize)]
pub struct RootStructTracks {
    pub href: Option<String>,
    pub items: Option<Vec<RootStructTracksItemsItem>>,
    pub limit: Option<i64>,
    pub next: Option<serde_json::Value>,
    pub offset: Option<i64>,
    pub previous: Option<serde_json::Value>,
    pub total: Option<i64>,
}