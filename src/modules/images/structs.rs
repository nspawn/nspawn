use {
    chrono::{NaiveDateTime, TimeZone},
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
    pub usage: Option<i64>,
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
            TimeZone::from_utc_datetime(
                &chrono::Local,
                &NaiveDateTime::from_timestamp_opt(self.created, 0).unwrap(),
            )
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
            TimeZone::from_utc_datetime(
                &chrono::Local,
                &NaiveDateTime::from_timestamp_opt(self.modified, 0).unwrap(),
            )
            .to_string()
        }
    }

    pub fn get_usage_in_gb(&mut self) -> String {
        if let Some(usage) = self.usage {
            format!("{:.2} GB", usage as f64 / 1_000_000_000.0)
        } else {
            "-".to_string()
        }
    }
}
