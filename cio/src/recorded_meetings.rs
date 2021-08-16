#![allow(clippy::from_over_into)]
use std::str::from_utf8;

use async_trait::async_trait;
use chrono::{offset::Utc, DateTime, Duration};
use google_drive::GoogleDrive;
use inflector::cases::kebabcase::to_kebab_case;
use macros::db;
use revai::RevAI;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zoom_api::types::GetAccountCloudRecordingResponseMeetingsFilesFileType;

use crate::{
    airtable::AIRTABLE_RECORDED_MEETINGS_TABLE,
    companies::Company,
    configs::User,
    core::UpdateAirtableRecord,
    db::Database,
    schema::{recorded_meetings, users},
    utils::truncate,
};

/// The data type for a recorded meeting.
#[db {
    new_struct_name = "RecordedMeeting",
    airtable_base = "misc",
    airtable_table = "AIRTABLE_RECORDED_MEETINGS_TABLE",
    match_on = {
        "google_event_id" = "String",
    },
}]
#[derive(Debug, Insertable, AsChangeset, PartialEq, Clone, JsonSchema, Deserialize, Serialize)]
#[table_name = "recorded_meetings"]
pub struct NewRecordedMeeting {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub video: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub chat_log_link: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub chat_log: String,
    #[serde(default)]
    pub is_recurring: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attendees: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transcript: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transcript_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub google_event_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub event_link: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub location: String,
    /// The CIO company ID.
    #[serde(default)]
    pub cio_company_id: i32,
}

/// Implement updating the Airtable record for a RecordedMeeting.
#[async_trait]
impl UpdateAirtableRecord<RecordedMeeting> for RecordedMeeting {
    async fn update_airtable_record(&mut self, record: RecordedMeeting) {
        if !record.transcript_id.is_empty() {
            self.transcript_id = record.transcript_id;
        }
        if !record.transcript.is_empty() {
            self.transcript = record.transcript;
        }

        self.transcript = truncate(&self.transcript, 100000);
    }
}

/// Sync the recorded meetings from zoom.
pub async fn refresh_zoom_recorded_meetings(db: &Database, company: &Company) {
    let zoom_auth = company.authenticate_zoom(db).await;
    if zoom_auth.is_none() {
        // Return early, this company does not use Zoom.
        return;
    }

    let mut zoom = zoom_auth.unwrap();

    // List all the recorded meetings.
    let recordings = zoom
        .cloud_recording()
        .get_all_account(
            "me", // we set account to me since the autorized user is an admin
            Some(Utc::now().checked_sub_signed(Duration::days(30)).unwrap()), // from: the max date range is a month.
            Some(Utc::now()), // to
        )
        .await
        .unwrap();

    if recordings.is_empty() {
        // Return early.
        return;
    }

    // Get our Google token.
    let token = company.authenticate_google(db).await;

    // Initialize the Google Drive client.
    let drive = GoogleDrive::new(token);

    // Get the shared drive.
    let shared_drive = drive.get_drive_by_name("Automated Documents").await.unwrap();

    // Create the folder for our zoom recordings.
    let recordings_folder_id = drive
        .create_folder(&shared_drive.id, "", "zoom_recordings")
        .await
        .unwrap();

    // We need the zoom token to download the URL.
    let at = zoom.refresh_access_token().await.unwrap();

    for meeting in recordings {
        if meeting.topic.is_empty() {
            // Continue early.
            println!("Meeting must have a topic!! {:?}", meeting);
            continue;
        }

        // Create the folder for our zoom recordings.
        let start_folder_id = drive
            .create_folder(
                &shared_drive.id,
                &recordings_folder_id,
                &meeting.start_time.unwrap().to_string(),
            )
            .await
            .unwrap();

        let mut transcript = String::new();
        let mut transcript_id = String::new();
        let mut video = String::new();
        let mut video_html_link = String::new();
        let mut chat_log_link = String::new();
        let mut chat_log = String::new();
        let mut end_time = Utc::now();

        // Move the recordings to the Google Drive folder.
        for recording in &meeting.recording_files {
            let file_type = recording.file_type.as_ref().unwrap();
            if *file_type == GetAccountCloudRecordingResponseMeetingsFilesFileType::Noop
                || *file_type == GetAccountCloudRecordingResponseMeetingsFilesFileType::FallthroughString
            {
                // Continue early.
                println!("[zoom] got bad recording file type: {:?}", recording);
                continue;
            }

            if let Some(status) = &recording.status {
                if *status != zoom_api::types::GetAccountCloudRecordingResponseMeetingsFilesStatus::Completed {
                    // Continue early.
                    println!("[zoom] got bad recording status: {:?}", recording);
                    continue;
                }
            }

            // Download the file to memory.
            println!(
                "[zoom] meeting {} -> downloading recording {}... This might take a bit...",
                meeting.topic, recording.download_url,
            );
            let resp = reqwest::get(&format!("{}?access_token={}", recording.download_url, at.access_token))
                .await
                .unwrap();
            let b = resp.bytes().await.unwrap();

            // Get the mime type.
            let mime_type = file_type.get_mime_type();

            // Upload the recording to Google drive.
            println!(
                "[zoom] uploading meeting {} recording to Google drive... This might take a bit...",
                meeting.topic
            );
            let drive_file = drive
                .create_or_update_file(
                    &shared_drive.id,
                    &start_folder_id,
                    &format!(
                        "{}{}",
                        to_kebab_case(meeting.topic.replace("'s", "").trim()),
                        file_type.to_extension()
                    ),
                    &mime_type,
                    &b,
                )
                .await
                .unwrap();

            match *file_type {
                GetAccountCloudRecordingResponseMeetingsFilesFileType::Mp4 => {
                    video = format!("https://drive.google.com/open?id={}", drive_file.id);
                    // TODO: get a better link
                    video_html_link = video.to_string();
                    end_time = DateTime::parse_from_rfc3339(&recording.recording_end)
                        .unwrap()
                        .with_timezone(&Utc);
                }
                GetAccountCloudRecordingResponseMeetingsFilesFileType::Transcript => {
                    transcript = from_utf8(&b).unwrap().to_string();
                    transcript_id = recording.id.to_string();
                }
                GetAccountCloudRecordingResponseMeetingsFilesFileType::Chat => {
                    chat_log_link = format!("https://drive.google.com/open?id={}", drive_file.id);
                    chat_log = from_utf8(&b).unwrap().to_string();
                }
                _ => (),
            }

            zoom.cloud_recording()
                .recording_delete_one(
                    &recording.meeting_id,
                    &recording.id,
                    zoom_api::types::RecordingDeleteAction::Trash,
                )
                .await
                .unwrap();
            println!(
            "[zoom] deleted meeting {} recording in Zoom since they are now in Google drive at https://drive.google.com/open?id={}",
                meeting.topic,
            drive_file.id
        );
        }

        let host = users::dsl::users
            .filter(
                users::dsl::zoom_id
                    .eq(meeting.host_id.to_string())
                    .and(users::dsl::cio_company_id.eq(company.id)),
            )
            .first::<User>(&db.conn())
            .unwrap();

        // Create the meeting in the database.
        let m = NewRecordedMeeting {
            name: meeting.topic.trim().to_string(),
            description: "".to_string(),
            start_time: meeting.start_time.unwrap(),
            end_time,
            video,
            chat_log_link,
            chat_log,
            is_recurring: false,
            attendees: vec![host.email.to_string()],
            transcript,
            transcript_id,
            location: format!("Meeting hosted by {}", host.full_name()),
            // We save the meeting ID here, even tho its in Zoom.
            // TODO: clean this up.
            google_event_id: meeting.uuid.to_string(),
            event_link: video_html_link,
            cio_company_id: company.id,
        };
        m.upsert(db).await;
    }
}

/// Sync the recorded meetings from Google.
pub async fn refresh_google_recorded_meetings(db: &Database, company: &Company) {
    RecordedMeetings::get_from_db(db, company.id).update_airtable(db).await;

    let mut gcal = company.authenticate_google_calendar(db).await;
    let revai = RevAI::new_from_env();

    // Get the list of our calendars.
    let calendars = gcal
        .calendar_list()
        .get_all(google_calendar::types::MinAccessRole::Noop, false, false)
        .await
        .unwrap();

    // Iterate over the calendars.
    for calendar in calendars {
        if calendar.id.ends_with(&company.gsuite_domain) {
            // We get a new token since likely our other has expired.
            gcal = company.authenticate_google_calendar(db).await;

            // Let's get all the events on this calendar and try and see if they
            // have a meeting recorded.
            println!("Getting events for {}", calendar.id);
            let events = gcal
                .events()
                .calendar_list_events(
                    &calendar.id, // Calendar id.
                    false,        // Deprecated and ignored.
                    "",           // iCalID
                    0,            // Max attendees, set to 0 to ignore.
                    google_calendar::types::OrderBy::StartTime,
                    &[],                      // private_extended_property
                    "",                       // q
                    &[],                      // shared_extended_property
                    true,                     // show_deleted
                    true,                     // show_hidden_invitations
                    true,                     // single_events
                    &Utc::now().to_rfc3339(), // time_max
                    "",                       // time_min
                    "",                       // time_zone
                    "",                       // updated_min
                )
                .await
                .unwrap();

            for event in events {
                // Let's check if there are attachments. We only care if there are attachments.
                if event.attachments.is_empty() {
                    // Continue early.
                    continue;
                }

                let mut attendees: Vec<String> = Default::default();
                for attendee in event.attendees {
                    if !attendee.resource {
                        attendees.push(attendee.email.to_string());
                    }
                }

                let mut video = "".to_string();
                let mut chat_log_link = "".to_string();
                for attachment in event.attachments {
                    if attachment.mime_type == "video/mp4" && attachment.title.starts_with(&event.summary) {
                        video = attachment.file_url.to_string();
                    }
                    if attachment.mime_type == "text/plain" && attachment.title.starts_with(&event.summary) {
                        chat_log_link = attachment.file_url.to_string();
                    }
                }

                if video.is_empty() {
                    // Continue early, we don't care.
                    continue;
                }

                let delegated_token = company.authenticate_google(db).await;
                let drive_client = GoogleDrive::new(delegated_token);

                // If we have a chat log, we should download it.
                let mut chat_log = "".to_string();
                if !chat_log_link.is_empty() {
                    // Download the file.
                    let contents = drive_client
                        .download_file_by_id(
                            chat_log_link
                                .trim_start_matches("https://drive.google.com/open?id=")
                                .trim_start_matches("https://drive.google.com/file/d/")
                                .trim_end_matches("/view?usp=drive_web"),
                        )
                        .await
                        .unwrap_or_default();
                    chat_log = from_utf8(&contents).unwrap_or_default().trim().to_string();
                }

                // Try to download the video.
                let video_contents = drive_client
                    .download_file_by_id(
                        video
                            .trim_start_matches("https://drive.google.com/open?id=")
                            .trim_start_matches("https://drive.google.com/file/d/")
                            .trim_end_matches("/view?usp=drive_web"),
                    )
                    .await
                    .unwrap_or_default();

                // Make sure the contents aren't empty.
                if video_contents.is_empty() {
                    // Continue early.
                    //continue;
                }

                let mut meeting = NewRecordedMeeting {
                    name: event.summary.trim().to_string(),
                    description: event.description.trim().to_string(),
                    start_time: event.start.unwrap().date_time.unwrap(),
                    end_time: event.end.unwrap().date_time.unwrap(),
                    video,
                    chat_log_link,
                    chat_log,
                    is_recurring: !event.recurring_event_id.is_empty(),
                    attendees,
                    transcript: "".to_string(),
                    transcript_id: "".to_string(),
                    location: event.location.to_string(),
                    google_event_id: event.id.to_string(),
                    event_link: event.html_link.to_string(),
                    cio_company_id: company.id,
                };

                // Let's try to get the meeting.
                let existing = RecordedMeeting::get_from_db(db, event.id.to_string());
                if let Some(m) = existing {
                    // Update the meeting.
                    meeting.transcript = m.transcript.to_string();
                    meeting.transcript_id = m.transcript_id.to_string();

                    // Get it from Airtable.
                    if let Some(existing_airtable) = m.get_existing_airtable_record(db).await {
                        if meeting.transcript.is_empty() {
                            meeting.transcript = existing_airtable.fields.transcript.to_string();
                        }
                        if meeting.transcript_id.is_empty() {
                            meeting.transcript_id = existing_airtable.fields.transcript_id.to_string();
                        }
                    }
                }

                // Upsert the meeting in the database.
                let mut db_meeting = meeting.upsert(db).await;

                if !video_contents.is_empty() {
                    // Only do this if we have the video contents.
                    // Check if we have a transcript id.
                    if db_meeting.transcript_id.is_empty() && db_meeting.transcript.is_empty() {
                        // If we don't have a transcript ID, let's post the video to be
                        // transcribed.
                        // Now let's upload it to rev.ai so it can start a job.
                        let job = revai.create_job(video_contents).await.unwrap();
                        // Set the transcript id.
                        db_meeting.transcript_id = job.id.to_string();
                        db_meeting.update(db).await;
                    } else {
                        // We have a transcript id, let's try and get the transcript if we don't have
                        // it already.
                        if db_meeting.transcript.is_empty() {
                            // Now let's try to get the transcript.
                            let transcript = revai
                                .get_transcript(&db_meeting.transcript_id)
                                .await
                                .unwrap_or_default();
                            db_meeting.transcript = transcript.trim().to_string();
                            db_meeting.update(db).await;
                        }
                    }
                }
            }
        }
    }
}

trait FileInfo {
    fn to_extension(&self) -> String;
    fn get_mime_type(&self) -> String;
}

impl FileInfo for GetAccountCloudRecordingResponseMeetingsFilesFileType {
    // Returns the extension for each file type.
    fn to_extension(&self) -> String {
        match self {
            GetAccountCloudRecordingResponseMeetingsFilesFileType::Mp4 => "-video.mp4".to_string(),
            GetAccountCloudRecordingResponseMeetingsFilesFileType::M4A => "-audio.m4a".to_string(),
            GetAccountCloudRecordingResponseMeetingsFilesFileType::Tb => ".json".to_string(),
            GetAccountCloudRecordingResponseMeetingsFilesFileType::Transcript => "-transcript.vtt".to_string(),
            GetAccountCloudRecordingResponseMeetingsFilesFileType::Chat => "-chat.txt".to_string(),
            GetAccountCloudRecordingResponseMeetingsFilesFileType::Cc => "-closed-captions.vtt".to_string(),
            GetAccountCloudRecordingResponseMeetingsFilesFileType::Csv => ".csv".to_string(),
            _ => "".to_string(),
        }
    }

    // Returns the mime type for each file type.
    fn get_mime_type(&self) -> String {
        match self {
            GetAccountCloudRecordingResponseMeetingsFilesFileType::Mp4 => "video/mp4".to_string(),
            GetAccountCloudRecordingResponseMeetingsFilesFileType::M4A => "audio/m4a".to_string(),
            GetAccountCloudRecordingResponseMeetingsFilesFileType::Tb => "application/json".to_string(),
            GetAccountCloudRecordingResponseMeetingsFilesFileType::Transcript => "text/vtt".to_string(),
            GetAccountCloudRecordingResponseMeetingsFilesFileType::Chat => "text/plain".to_string(),
            GetAccountCloudRecordingResponseMeetingsFilesFileType::Cc => "text/vtt".to_string(),
            GetAccountCloudRecordingResponseMeetingsFilesFileType::Csv => "text/csv".to_string(),
            _ => "".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        companies::Companys,
        db::Database,
        recorded_meetings::{refresh_google_recorded_meetings, refresh_zoom_recorded_meetings},
    };

    #[ignore]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_zoom_recorded_meetings() {
        // Initialize our database.
        let db = Database::new();
        let companies = Companys::get_from_db(&db, 1);
        // Iterate over the companies and update.
        for company in companies {
            refresh_zoom_recorded_meetings(&db, &company).await;
        }
    }

    #[ignore]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_google_recorded_meetings() {
        // Initialize our database.
        let db = Database::new();
        let companies = Companys::get_from_db(&db, 1);
        // Iterate over the companies and update.
        for company in companies {
            refresh_google_recorded_meetings(&db, &company).await;
        }
    }
}
