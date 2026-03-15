use crate::model::entry::Entry;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub created_at: i64,
    pub root_dir: String,
    pub entries: Vec<Entry>,
}

impl Snapshot {
    pub const CURRENT_VERSION: u32 = 1;
}
