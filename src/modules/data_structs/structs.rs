use serde::{Deserialize, Serialize};

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

pub type InterfacesConfigs = Vec<InterfaceConfig>;

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InterfaceConfig {
    pub ifindex: i64,
    pub ifname: String,
    pub flags: Vec<String>,
    pub mtu: i64,
    pub qdisc: String,
    pub operstate: String,
    pub group: String,
    pub txqlen: Option<i64>,
    #[serde(rename = "link_type")]
    pub link_type: String,
    pub address: String,
    pub broadcast: String,
    #[serde(rename = "addr_info")]
    pub addr_info: Vec<AddrInfo>,
    pub master: Option<String>,
    pub permaddr: Option<String>,
    #[serde(rename = "link_index")]
    pub link_index: Option<i64>,
    #[serde(rename = "link_netnsid")]
    pub link_netnsid: Option<i64>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddrInfo {
    pub family: String,
    pub local: String,
    pub prefixlen: i64,
    pub scope: String,
    pub label: Option<String>,
    #[serde(rename = "valid_life_time")]
    pub valid_life_time: i64,
    #[serde(rename = "preferred_life_time")]
    pub preferred_life_time: i64,
    pub metric: Option<i64>,
    pub broadcast: Option<String>,
    pub dynamic: Option<bool>,
}
