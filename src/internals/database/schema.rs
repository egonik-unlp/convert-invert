// @generated automatically by Diesel CLI.

pub mod sql_types {
    #[derive(diesel::sql_types::SqlType)]
    #[diesel(postgres_type(name = "reject_reason"))]
    pub struct RejectReason;
}

diesel::table! {
    download_requests (id) {
        id -> Int4,
        user_id -> Nullable<Int4>,
        source_kind -> Varchar,
        source_id -> Varchar,
        source_name -> Varchar,
        options -> Jsonb,
        track_count -> Int4,
        created_at -> Timestamptz,
    }
}

diesel::table! {
    downloadable_files (id) {
        id -> Int4,
        filename -> Varchar,
        username -> Varchar,
        size -> Int8,
    }
}

diesel::table! {
    downloaded_file (id) {
        id -> Int4,
        filename -> Varchar,
        track -> Nullable<Int4>,
        local_path -> Nullable<Varchar>,
    }
}

diesel::table! {
    judgements (id) {
        id -> Int4,
        submission -> Int4,
        judge -> Varchar,
        model -> Nullable<Varchar>,
        label -> Nullable<Varchar>,
        score -> Float4,
        accepted -> Bool,
        latency_ms -> Int4,
        prompt_version -> Nullable<Varchar>,
        raw -> Nullable<Text>,
        error -> Nullable<Text>,
        created_at -> Timestamptz,
    }
}

diesel::table! {
    judge_submissions (id) {
        id -> Int4,
        track -> Int4,
        query -> Int4,
        score -> Nullable<Float4>,
        relative_mi_score -> Nullable<Float4>,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::RejectReason;

    rejected_track (id) {
        id -> Int4,
        track -> Int4,
        reason -> RejectReason,
        value -> Nullable<Varchar>,
    }
}

diesel::table! {
    manual_accepts (id) {
        id -> Int4,
        track -> Int4,
        downloadable_file -> Int4,
        user_id -> Nullable<Int4>,
        created_at -> Timestamptz,
    }
}

diesel::table! {
    retry_request (id) {
        id -> Int4,
        request -> Int4,
        retry_attempts -> Int4,
        failed_download_result -> Int4,
    }
}

diesel::table! {
    search_items (id) {
        id -> Int4,
        track_id -> Varchar,
        track -> Varchar,
        artist -> Varchar,
        album -> Varchar,
        playlist_id -> Nullable<Varchar>,
        playlist_name -> Nullable<Varchar>,
        request_id -> Nullable<Int4>,
    }
}

diesel::table! {
    sessions (id) {
        id -> Int4,
        user_id -> Int4,
        token_hash -> Varchar,
        user_agent -> Nullable<Varchar>,
        created_at -> Timestamptz,
        last_used_at -> Timestamptz,
        expires_at -> Timestamptz,
    }
}

diesel::table! {
    spotify_tokens (user_id) {
        user_id -> Int4,
        refresh_token_enc -> Bytea,
        access_token_enc -> Nullable<Bytea>,
        access_expires_at -> Nullable<Timestamptz>,
        scopes -> Varchar,
    }
}

diesel::table! {
    users (id) {
        id -> Int4,
        spotify_id -> Nullable<Varchar>,
        email -> Varchar,
        display_name -> Nullable<Varchar>,
        avatar_url -> Nullable<Varchar>,
        role -> Varchar,
        status -> Varchar,
        locale -> Nullable<Varchar>,
        created_at -> Timestamptz,
        last_seen_at -> Nullable<Timestamptz>,
    }
}

diesel::joinable!(judgements -> judge_submissions (submission));
diesel::joinable!(judge_submissions -> downloadable_files (query));
diesel::joinable!(judge_submissions -> search_items (track));
diesel::joinable!(downloaded_file -> search_items (track));
diesel::joinable!(rejected_track -> judge_submissions (track));
diesel::joinable!(retry_request -> downloadable_files (failed_download_result));
diesel::joinable!(retry_request -> judge_submissions (request));
diesel::joinable!(download_requests -> users (user_id));
diesel::joinable!(manual_accepts -> downloadable_files (downloadable_file));
diesel::joinable!(manual_accepts -> search_items (track));
diesel::joinable!(manual_accepts -> users (user_id));
diesel::joinable!(search_items -> download_requests (request_id));
diesel::joinable!(sessions -> users (user_id));
diesel::joinable!(spotify_tokens -> users (user_id));

diesel::allow_tables_to_appear_in_same_query!(
    download_requests,
    downloadable_files,
    downloaded_file,
    judge_submissions,
    judgements,
    manual_accepts,
    rejected_track,
    retry_request,
    search_items,
    sessions,
    spotify_tokens,
    users,
);
