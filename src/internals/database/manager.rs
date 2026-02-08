use anyhow::Context;
use diesel::{PgConnection, dsl::insert_into, prelude::*};

use crate::internals::context::context_manager::{RejectedTrack, RetryRequest, Track};
use crate::internals::database::{model, schema};
use crate::internals::search::search_manager::{
    DownloadableFile as RuntimeDownloadableFile, JudgeSubmission as RuntimeJudgeSubmission,
    SearchItem as RuntimeSearchItem,
};
pub struct DatabaseManager<'a> {
    pub connection: &'a mut PgConnection,
}

impl<'a> DatabaseManager<'a> {
    pub fn new(connection: &'a mut PgConnection) -> Self {
        Self { connection }
    }

    fn insert_search_item(
        connection: &mut PgConnection,
        search_item: &RuntimeSearchItem,
    ) -> anyhow::Result<i32> {
        let value = model::NewSearchItemRow::from(search_item);
        let inserted_id = insert_into(schema::search_items::table)
            .values(&value)
            .returning(schema::search_items::id)
            .get_result(connection)
            .context("Insert search item")?;
        Ok(inserted_id)
    }

    fn insert_downloadable_file(
        connection: &mut PgConnection,
        downloadable_file: &RuntimeDownloadableFile,
    ) -> anyhow::Result<i32> {
        let value = model::NewDownloadableFileRow::from(downloadable_file);
        let inserted_id = insert_into(schema::downloadable_files::table)
            .values(&value)
            .returning(schema::downloadable_files::id)
            .get_result(connection)
            .context("Insert downloadable file")?;
        Ok(inserted_id)
    }

    fn insert_judge_submission(
        connection: &mut PgConnection,
        judge_submission: &RuntimeJudgeSubmission,
    ) -> anyhow::Result<i32> {
        let track_id = Self::insert_search_item(connection, &judge_submission.track)?;
        let query_id = Self::insert_downloadable_file(connection, &judge_submission.query)?;
        let value = model::NewJudgeSubmissionRow {
            track: track_id,
            query: query_id,
        };
        let inserted_id = insert_into(schema::judge_submissions::table)
            .values(&value)
            .returning(schema::judge_submissions::id)
            .get_result(connection)
            .context("Insert judge submission")?;
        Ok(inserted_id)
    }

    fn insert_retry_request(
        connection: &mut PgConnection,
        retry_request: &RetryRequest,
    ) -> anyhow::Result<()> {
        let request_id = Self::insert_judge_submission(connection, &retry_request.request)?;
        let failed_download_result =
            Self::insert_downloadable_file(connection, &retry_request.failed_download_result)?;
        let value = model::NewRetryRequestRow {
            request: request_id,
            retry_attempts: i32::from(retry_request.retry_attempts),
            failed_download_result,
        };
        insert_into(schema::retry_request::table)
            .values(&value)
            .execute(connection)
            .context("Insert retry request")?;
        Ok(())
    }

    fn insert_rejected_track(
        connection: &mut PgConnection,
        rejected_track: &RejectedTrack,
    ) -> anyhow::Result<()> {
        let (judge_submission, _) = rejected_track.parts();
        let track_id = Self::insert_judge_submission(connection, judge_submission)?;
        let value = model::NewRejectedTrackRow::from_runtime(track_id, rejected_track);
        insert_into(schema::rejected_track::table)
            .values(&value)
            .execute(connection)
            .context("Insert rejected track")?;
        Ok(())
    }

    pub fn load_item_to_database(&mut self, item: &Track) -> anyhow::Result<()> {
        self.connection
            .transaction::<_, anyhow::Error, _>(|connection| {
                match item {
                    Track::Query(search_item) => {
                        Self::insert_search_item(connection, search_item)?;
                    }
                    Track::Result(judge_submission) | Track::Downloadable(judge_submission) => {
                        Self::insert_judge_submission(connection, judge_submission)?;
                    }
                    Track::File(downloaded_file) => {
                        let value = model::NewDownloadedFileRow::from(downloaded_file);
                        insert_into(schema::downloaded_file::table)
                            .values(value)
                            .execute(connection)
                            .context("Insert downloaded file")?;
                    }
                    Track::Retry(retry_request) => {
                        Self::insert_retry_request(connection, retry_request)?;
                    }
                    Track::Reject(rejected_track) => {
                        Self::insert_rejected_track(connection, rejected_track)?;
                    }
                }
                Ok(())
            })
            .context("Persist track into database")?;
        Ok(())
    }
}
