use chrono::{NaiveDateTime, Utc};

use {
    chrono::DateTime,
    serde::{Deserialize, Serialize},
};

pub type Images = Vec<Image>;

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Image {
    pub name: String,
    #[serde(rename = "type")]
    pub type_field: String,
    pub ro: bool,
    pub usage: Option<String>,
    pub created: i64,
    pub modified: i64,
}

impl Image {
    pub fn created_to_timestamp(&mut self) -> String {
        // Convert the created and modified times to seconds since epoch because the chrono crate implements
        // conversion to timestamp from milliseconds
        self.created /= 1_000_000;

        if self.created == 0 {
            "-".to_string()
        } else {
            DateTime::<Utc>::from_utc(NaiveDateTime::from_timestamp(self.created, 0), Utc)
                .to_string()
        }
    }

    pub fn modified_to_timestamp(&mut self) -> String {
        // Convert the created and modified times to seconds since epoch because the chrono crate implements
        // conversion to timestamp from milliseconds
        self.modified /= 1_000_000;

        if self.modified == 0 {
            "-".to_string()
        } else {
            DateTime::<Utc>::from_utc(NaiveDateTime::from_timestamp(self.modified, 0), Utc)
                .to_string()
        }
    }
}
