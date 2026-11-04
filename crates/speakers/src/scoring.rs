//! Exact interval DER: zero collar, overlapping reference speech included, optimal global mapping.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Annotation {
    pub start: f64,
    pub end: f64,
    pub speaker: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DerScore {
    pub reference_speaker_seconds: f64,
    pub miss_seconds: f64,
    pub false_alarm_seconds: f64,
    pub confusion_seconds: f64,
    pub der_percent: f64,
    pub mapping: BTreeMap<String, String>,
}
pub fn parse_rttm(text: &str) -> Result<Vec<Annotation>> {
    let mut result = vec![];
    for line in text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
    {
        let fields: Vec<_> = line.split_whitespace().collect();
        ensure!(
            fields.len() >= 8 && fields[0] == "SPEAKER",
            "invalid RTTM line"
        );
        let start = fields[3].parse::<f64>()?;
        let duration = fields[4].parse::<f64>()?;
        ensure!(
            start.is_finite() && start >= 0. && duration.is_finite() && duration >= 0.,
            "invalid RTTM time"
        );
        result.push(Annotation {
            start,
            end: start + duration,
            speaker: fields[7].into(),
        });
    }
    Ok(result)
}
pub fn score_der(
    reference: &[Annotation],
    hypothesis: &[Annotation],
    start: f64,
    end: f64,
) -> Result<DerScore> {
    ensure!(
        start.is_finite() && end.is_finite() && end > start,
        "invalid scoring region"
    );
    let refs: Vec<_> = reference
        .iter()
        .filter(|a| a.end > start && a.start < end)
        .map(|a| a.speaker.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let hyps: Vec<_> = hypothesis
        .iter()
        .filter(|a| a.end > start && a.start < end)
        .map(|a| a.speaker.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    ensure!(
        refs.len() <= 8 && hyps.len() <= 8,
        "scorer is bounded to eight speakers"
    );
    let mut cuts = vec![start, end];
    for a in reference.iter().chain(hypothesis) {
        if a.end > start && a.start < end {
            cuts.push(a.start.max(start));
            cuts.push(a.end.min(end));
        }
    }
    cuts.sort_by(f64::total_cmp);
    cuts.dedup();
    let mut weight = vec![vec![0.; hyps.len()]; refs.len()];
    let mut atoms = vec![];
    for pair in cuts.windows(2) {
        let at = (pair[0] + pair[1]) / 2.;
        let duration = pair[1] - pair[0];
        let r: Vec<_> = refs
            .iter()
            .enumerate()
            .filter(|(_, who)| {
                reference
                    .iter()
                    .any(|a| &a.speaker == *who && a.start <= at && at < a.end)
            })
            .map(|(i, _)| i)
            .collect();
        let h: Vec<_> = hyps
            .iter()
            .enumerate()
            .filter(|(_, who)| {
                hypothesis
                    .iter()
                    .any(|a| &a.speaker == *who && a.start <= at && at < a.end)
            })
            .map(|(i, _)| i)
            .collect();
        for &i in &r {
            for &j in &h {
                weight[i][j] += duration;
            }
        }
        atoms.push((duration, r, h));
    }
    fn assign(
        row: usize,
        used: u16,
        weight: &[Vec<f64>],
        current: &mut Vec<Option<usize>>,
        best: &mut (f64, Vec<Option<usize>>),
        sum: f64,
    ) {
        if row == weight.len() {
            if sum > best.0 {
                *best = (sum, current.clone());
            }
            return;
        }
        current.push(None);
        assign(row + 1, used, weight, current, best, sum);
        current.pop();
        for j in 0..weight[row].len() {
            if used & (1 << j) != 0 {
                continue;
            }
            current.push(Some(j));
            assign(
                row + 1,
                used | (1 << j),
                weight,
                current,
                best,
                sum + weight[row][j],
            );
            current.pop();
        }
    }
    let mut best = (-1., vec![]);
    assign(0, 0, &weight, &mut vec![], &mut best, 0.);
    let mut reference_speaker_seconds = 0.;
    let mut miss_seconds = 0.;
    let mut false_alarm_seconds = 0.;
    let mut confusion_seconds = 0.;
    for (duration, r, h) in atoms {
        let nr = r.len();
        let nh = h.len();
        let correct = r
            .iter()
            .filter(|&&i| best.1[i].is_some_and(|j| h.contains(&j)))
            .count();
        reference_speaker_seconds += nr as f64 * duration;
        miss_seconds += nr.saturating_sub(nh) as f64 * duration;
        false_alarm_seconds += nh.saturating_sub(nr) as f64 * duration;
        confusion_seconds += (nr.min(nh) - correct) as f64 * duration;
    }
    ensure!(
        reference_speaker_seconds > 0.,
        "no reference speech in scoring region"
    );
    let mapping = refs
        .iter()
        .enumerate()
        .filter_map(|(i, r)| best.1[i].map(|j| (hyps[j].clone(), r.clone())))
        .collect();
    Ok(DerScore {
        reference_speaker_seconds,
        miss_seconds,
        false_alarm_seconds,
        confusion_seconds,
        der_percent: 100. * (miss_seconds + false_alarm_seconds + confusion_seconds)
            / reference_speaker_seconds,
        mapping,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn a(start: f64, end: f64, speaker: &str) -> Annotation {
        Annotation {
            start,
            end,
            speaker: speaker.into(),
        }
    }
    #[test]
    fn mapping_overlap_and_false_alarm_are_scored() {
        let refs = vec![a(0., 2., "A"), a(1., 3., "B")];
        let perfect = vec![a(0., 2., "2"), a(1., 3., "1")];
        assert_eq!(score_der(&refs, &perfect, 0., 3.).unwrap().der_percent, 0.);
        let missing = vec![a(0., 2., "2")];
        assert_eq!(score_der(&refs, &missing, 0., 3.).unwrap().miss_seconds, 2.);
        let mut false_alarm = perfect.clone();
        false_alarm.push(a(0., 1., "3"));
        let score = score_der(&refs, &false_alarm, 0., 3.).unwrap();
        assert_eq!(score.false_alarm_seconds, 1.);
        assert_eq!(score.der_percent, 25.);
    }
}
