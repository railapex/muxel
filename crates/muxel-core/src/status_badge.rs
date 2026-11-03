//! Completion recency shared by lifecycle indicators. Timestamps are Unix milliseconds.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DoneAge {
    JustFinished,
    Recent,
    Today,
    Older,
}

impl DoneAge {
    pub const ALL: [Self; 4] = [Self::JustFinished, Self::Recent, Self::Today, Self::Older];

    /// Strength of the completion colour, never of the label's opacity.
    pub fn strength(self) -> f32 {
        match self {
            Self::JustFinished => 1.0,
            Self::Recent => 0.6,
            Self::Today => 0.3,
            Self::Older => 0.1,
        }
    }
}

const EDGES: [i64; 3] = [10 * 60_000, 60 * 60_000, 24 * 60 * 60_000];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SampledActivityKey {
    Completion(Option<i64>),
    Label(String),
}

/// Presentation changes observed by the background lifecycle sampler. Done age
/// is refreshed by visible fade boundaries, never by coarse-label polling.
pub fn sampled_activity_key(
    state: crate::AgentActivityState,
    activity: &crate::AgentActivity,
    now: i64,
) -> SampledActivityKey {
    if state == crate::AgentActivityState::Done {
        SampledActivityKey::Completion(activity.completed_at)
    } else {
        SampledActivityKey::Label(crate::agent_activity_label(state, activity, now))
    }
}

/// Unknown completion times use the quiet style. Future timestamps are treated
/// as just finished (e.g. after clock correction), without underflow.
pub fn done_age(completed_at: Option<i64>, now: i64) -> DoneAge {
    let Some(at) = completed_at else {
        return DoneAge::Older;
    };
    let age = now.saturating_sub(at).max(0);
    match EDGES.iter().position(|edge| age < *edge) {
        Some(0) => DoneAge::JustFinished,
        Some(1) => DoneAge::Recent,
        Some(2) => DoneAge::Today,
        _ => DoneAge::Older,
    }
}

/// The next change in completion style, if the recorded time has another band.
/// Saturation cannot produce a deadline at or before `now`.
pub fn next_done_fade(completed_at: Option<i64>, now: i64) -> Option<i64> {
    let at = completed_at?;
    EDGES
        .iter()
        .map(|edge| at.saturating_add(*edge))
        .find(|edge| *edge > now)
}

#[cfg(test)]
mod tests {
    use super::{DoneAge, done_age, next_done_fade, sampled_activity_key};

    #[test]
    fn sampled_done_key_ignores_elapsed_time() {
        use crate::{AgentActivity, AgentActivityState};
        let activity = AgentActivity {
            completed_at: Some(1_000),
            ..Default::default()
        };
        let first = sampled_activity_key(AgentActivityState::Done, &activity, 121_000);
        for now in [181_000, 601_000, 3_601_000, 86_401_000] {
            assert_eq!(
                sampled_activity_key(AgentActivityState::Done, &activity, now),
                first
            );
        }
        let completed_again = AgentActivity {
            completed_at: Some(2_000),
            ..Default::default()
        };
        assert_ne!(
            sampled_activity_key(AgentActivityState::Done, &completed_again, 181_000),
            first
        );
        let blocked = AgentActivity {
            blocked_at: Some(1_000),
            ..Default::default()
        };
        assert_ne!(
            sampled_activity_key(AgentActivityState::Blocked, &blocked, 121_000),
            sampled_activity_key(AgentActivityState::Blocked, &blocked, 181_000)
        );
    }

    #[test]
    fn completion_bands_change_at_the_documented_edges() {
        let at = 1_000;
        for (age, band, next) in [
            (0, DoneAge::JustFinished, Some(600_000)),
            (599_999, DoneAge::JustFinished, Some(600_000)),
            (600_000, DoneAge::Recent, Some(3_600_000)),
            (3_599_999, DoneAge::Recent, Some(3_600_000)),
            (3_600_000, DoneAge::Today, Some(86_400_000)),
            (86_399_999, DoneAge::Today, Some(86_400_000)),
            (86_400_000, DoneAge::Older, None),
        ] {
            assert_eq!(done_age(Some(at), at + age), band);
            assert_eq!(
                next_done_fade(Some(at), at + age),
                next.map(|edge| at + edge)
            );
        }
    }

    #[test]
    fn missing_future_and_extreme_times_are_safe() {
        assert_eq!(done_age(None, 0), DoneAge::Older);
        assert_eq!(next_done_fade(None, 0), None);
        assert_eq!(done_age(Some(100), 0), DoneAge::JustFinished);
        assert_eq!(next_done_fade(Some(100), 0), Some(600_100));
        assert_eq!(done_age(Some(i64::MIN), i64::MAX), DoneAge::Older);
        assert_eq!(next_done_fade(Some(i64::MIN), i64::MAX), None);
        assert_eq!(next_done_fade(Some(i64::MAX), i64::MAX), None);
    }
}
