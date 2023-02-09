use serde::{Deserialize, Serialize};

pub type Machines = Vec<Machine>;

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Machine {
    pub machine: Option<String>,
    pub class: Option<String>,
    pub service: Option<String>,
    pub os: Option<String>,
    pub version: Option<String>,
    pub addresses: Option<String>,
}
