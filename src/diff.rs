use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ScoreDiff {
    pub base: Option<i64>,
    pub candidate: Option<i64>,
    pub delta: Option<i64>,
    pub delta_percent: Option<f64>,
}

pub fn score_diff(base: Option<i64>, candidate: Option<i64>) -> ScoreDiff {
    ScoreDiff {
        base,
        candidate,
        delta: base
            .zip(candidate)
            .map(|(base, candidate)| candidate - base),
        delta_percent: base
            .zip(candidate)
            .filter(|(base, _)| *base != 0)
            .map(|(base, candidate)| (candidate - base) as f64 / base as f64 * 100.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_base_has_no_percentage_delta() {
        let score = score_diff(Some(0), Some(10));
        assert_eq!(score.delta, Some(10));
        assert_eq!(score.delta_percent, None);
    }
}
