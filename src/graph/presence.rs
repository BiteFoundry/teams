//! Microsoft Graph v1.0 Presence endpoints (`/me/presence`, `/communications/getPresencesByUserId`).

use std::collections::HashSet;

use anyhow::Result;
use serde::Deserialize;

use super::{client::GraphListPage, GraphClient};
use crate::models::PresenceStatus;

const MAX_PRESENCE_BATCH_IDS: usize = 650;

#[derive(Debug, Deserialize)]
struct GraphPresence {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    availability: Option<String>,
    #[serde(default)]
    activity: Option<String>,
}

impl From<GraphPresence> for PresenceStatus {
    fn from(p: GraphPresence) -> Self {
        Self {
            user_id: p.id.unwrap_or_default(),
            availability: p
                .availability
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "PresenceUnknown".to_string()),
            activity: p
                .activity
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "PresenceUnknown".to_string()),
        }
    }
}

impl GraphClient {
    pub async fn get_my_presence(&self) -> Result<PresenceStatus> {
        let raw: GraphPresence = self
            .get_json("/me/presence?$select=id,availability,activity")
            .await?;
        let mut status: PresenceStatus = raw.into();
        if status.user_id.is_empty() {
            if let Some(me) = self.resolve_my_user_id().await {
                status.user_id = me;
            }
        }
        Ok(status)
    }

    pub async fn get_presences_by_user_ids(
        &self,
        user_ids: &[String],
    ) -> Result<Vec<PresenceStatus>> {
        let mut seen = HashSet::new();
        let cleaned: Vec<String> = user_ids
            .iter()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty() && seen.insert(id.clone()))
            .collect();

        if cleaned.is_empty() {
            return Ok(Vec::new());
        }

        let mut all = Vec::with_capacity(cleaned.len());
        for chunk in cleaned.chunks(MAX_PRESENCE_BATCH_IDS) {
            let body = serde_json::json!({ "ids": chunk });
            let page: GraphListPage<GraphPresence> = self
                .post_json("/communications/getPresencesByUserId", &body)
                .await?;
            all.extend(page.value.into_iter().map(PresenceStatus::from));
        }

        Ok(all)
    }

    pub async fn set_my_presence(
        &self,
        availability: &str,
        activity: &str,
        expiration_mins: Option<u32>,
    ) -> Result<()> {
        let avail = availability.trim();
        let act = activity.trim();

        if avail.eq_ignore_ascii_case("Reset")
            || avail.eq_ignore_ascii_case("Clear")
            || act.eq_ignore_ascii_case("Reset")
            || act.eq_ignore_ascii_case("Clear")
        {
            return self
                .post_empty("/me/presence/clearUserPreferredPresence")
                .await;
        }

        let mut body = serde_json::json!({
            "availability": avail,
            "activity": act,
        });
        if let Some(mins) = expiration_mins {
            body["expirationDuration"] =
                serde_json::Value::String(format_iso8601_duration_mins(mins));
        }
        self.post_json_empty("/me/presence/setUserPreferredPresence", &body)
            .await
    }
}

pub(crate) fn format_iso8601_duration_mins(mins: u32) -> String {
    let m = mins.max(1);
    if m.is_multiple_of(60) {
        format!("PT{}H", m / 60)
    } else {
        format!("PT{}M", m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_iso8601_duration_mins() {
        assert_eq!(format_iso8601_duration_mins(60), "PT1H");
        assert_eq!(format_iso8601_duration_mins(120), "PT2H");
        assert_eq!(format_iso8601_duration_mins(30), "PT30M");
        assert_eq!(format_iso8601_duration_mins(0), "PT1M");
    }

    #[test]
    fn test_graph_presence_deserialization() {
        let raw: GraphPresence = serde_json::from_value(serde_json::json!({
            "id": "u-1",
            "availability": "Busy",
            "activity": "InACall"
        }))
        .unwrap();
        let status: PresenceStatus = raw.into();
        assert_eq!(status.user_id, "u-1");
        assert_eq!(status.availability, "Busy");
        assert_eq!(status.activity, "InACall");
    }
}
