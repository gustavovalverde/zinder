//! Deterministic Cipherscan blend scoring and split-plan construction.

use std::{cmp::Reverse, collections::HashSet};

use serde::Serialize;

const ZATOSHIS_PER_ZEC: f64 = 100_000_000.0;
const MINIMUM_SPLIT_PIECE_ZAT: u64 = 10_000_000;
const MAX_SPLIT_PIECES: usize = 12;
const MAX_NEARBY_AMOUNTS: usize = 10;
const IGNORED_REMAINDER_ZAT: u64 = 100;

/// Cipherscan's display label for a deterministic blend score.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) enum BlendLabel {
    #[serde(rename = "Blends well")]
    BlendsWell,
    Moderate,
    #[serde(rename = "Stands out")]
    StandsOut,
}

/// An exact 30-day count for a nearby amount candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NearbyCandidateCount {
    pub(crate) amount_zat: u64,
    pub(crate) count_30d: u64,
}

/// A Cipherscan-compatible nearby popular amount.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NearbyPopularAmount {
    pub(crate) amount: f64,
    pub(crate) count: u64,
    pub(crate) blend_score: u8,
}

/// An exact 30-day count for a discovered split denomination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SplitCandidateCount {
    pub(crate) amount_zat: u64,
    pub(crate) count_30d: u64,
}

/// One piece in a Cipherscan-compatible split plan.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SplitPiece {
    pub(crate) amount: f64,
    pub(crate) blend_score: u8,
    pub(crate) blend_label: BlendLabel,
    pub(crate) count_30d: u64,
    pub(crate) is_remainder: bool,
}

/// One Cipherscan-compatible split plan.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SplitPlan {
    pub(crate) piece_count: usize,
    pub(crate) pieces: Vec<SplitPiece>,
    pub(crate) min_blend_score: u8,
    pub(crate) avg_blend_score: u8,
    pub(crate) overall_label: BlendLabel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) recommended: Option<bool>,
}

#[derive(Clone, Copy)]
struct ScoredDenomination {
    amount_zat: u64,
    count_30d: u64,
    blend_score: u8,
}

#[derive(Clone, Copy)]
struct GreedyPiece {
    amount_zat: u64,
    count_30d: u64,
    blend_score: u8,
    is_remainder: bool,
}

/// Computes Cipherscan's stepwise score from an exact 30-day match count.
pub(crate) const fn compute_blend_score(count_30d: u64) -> u8 {
    match count_30d {
        500.. => 95,
        200..=499 => 85,
        100..=199 => 75,
        50..=99 => 65,
        25..=49 => 50,
        10..=24 => 40,
        5..=9 => 25,
        1..=4 => 10,
        0 => 0,
    }
}

/// Returns Cipherscan's display label for `score`.
pub(crate) const fn blend_label(score: u8) -> BlendLabel {
    match score {
        70.. => BlendLabel::BlendsWell,
        40..=69 => BlendLabel::Moderate,
        0..=39 => BlendLabel::StandsOut,
    }
}

/// Scores, orders, and truncates already-fetched nearby candidate counts.
pub(crate) fn nearby_popular_amounts(
    target_amount_zat: u64,
    candidates: impl IntoIterator<Item = NearbyCandidateCount>,
) -> Vec<NearbyPopularAmount> {
    let mut candidates = candidates
        .into_iter()
        .filter(|candidate| candidate.count_30d >= 1)
        .collect::<Vec<_>>();

    candidates.sort_by_key(|candidate| {
        (
            Reverse(compute_blend_score(candidate.count_30d)),
            candidate.amount_zat.abs_diff(target_amount_zat),
        )
    });
    candidates.truncate(MAX_NEARBY_AMOUNTS);

    candidates
        .into_iter()
        .map(|candidate| NearbyPopularAmount {
            amount: round_decimal(zec_from_zatoshis(candidate.amount_zat), 4),
            count: candidate.count_30d,
            blend_score: compute_blend_score(candidate.count_30d),
        })
        .collect()
}

/// Builds Cipherscan split plans from exact original, denomination, and remainder counts.
///
/// `count_30d_by_amount` supplies exact counts for possible greedy remainders. Missing
/// remainder counts match Cipherscan's effective zero-count behavior.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::suboptimal_flops,
    reason = "Cipherscan computes weighted scores as JavaScript numbers and returns Math.round as an integer score"
)]
pub(crate) fn build_split_plans(
    target_amount_zat: u64,
    original_count_30d: u64,
    candidates: impl IntoIterator<Item = SplitCandidateCount>,
    count_30d_by_amount: impl Fn(u64) -> Option<u64>,
) -> Vec<SplitPlan> {
    let original_score = compute_blend_score(original_count_30d);
    let scored_denominations = candidates
        .into_iter()
        .map(|candidate| ScoredDenomination {
            amount_zat: candidate.amount_zat,
            count_30d: candidate.count_30d,
            blend_score: compute_blend_score(candidate.count_30d),
        })
        .filter(|candidate| candidate.blend_score > original_score)
        .collect::<Vec<_>>();

    let mut by_amount = scored_denominations.clone();
    by_amount.sort_by_key(|candidate| Reverse(candidate.amount_zat));

    let mut by_score = scored_denominations;
    by_score.sort_by_key(|candidate| (Reverse(candidate.blend_score), candidate.amount_zat));

    let max_pieces = usize::try_from(
        target_amount_zat
            .div_ceil(MINIMUM_SPLIT_PIECE_ZAT)
            .min(MAX_SPLIT_PIECES as u64),
    )
    .unwrap_or(MAX_SPLIT_PIECES);
    let mut signatures = HashSet::new();
    let mut plans = Vec::new();

    for denominations in [&by_amount, &by_score] {
        for piece_limit in 2..=max_pieces {
            let mut pieces = greedy_split(target_amount_zat, denominations, piece_limit);
            if pieces.len() <= 1 {
                continue;
            }

            let mut signature = pieces
                .iter()
                .map(|piece| piece.amount_zat)
                .collect::<Vec<_>>();
            signature.sort_unstable_by(|left, right| right.cmp(left));
            if !signatures.insert(signature) {
                continue;
            }

            for piece in pieces.iter_mut().filter(|piece| piece.is_remainder) {
                piece.count_30d = count_30d_by_amount(piece.amount_zat).unwrap_or(0);
                piece.blend_score = compute_blend_score(piece.count_30d);
            }

            let min_blend_score = pieces
                .iter()
                .map(|piece| piece.blend_score)
                .min()
                .unwrap_or(0);
            if min_blend_score <= original_score {
                continue;
            }

            let weighted_average = pieces.iter().fold(0.0, |sum, piece| {
                sum + f64::from(piece.blend_score)
                    * (piece.amount_zat as f64 / target_amount_zat as f64)
            });
            plans.push(SplitPlan {
                piece_count: pieces.len(),
                pieces: pieces
                    .into_iter()
                    .map(|piece| SplitPiece {
                        amount: round_decimal(zec_from_zatoshis(piece.amount_zat), 8),
                        blend_score: piece.blend_score,
                        blend_label: blend_label(piece.blend_score),
                        count_30d: piece.count_30d,
                        is_remainder: piece.is_remainder,
                    })
                    .collect(),
                min_blend_score,
                avg_blend_score: weighted_average.round() as u8,
                overall_label: blend_label(min_blend_score),
                recommended: None,
            });
        }
    }

    plans.sort_by_key(|plan| (Reverse(plan.min_blend_score), plan.piece_count));
    if let Some(recommended) = plans.first_mut() {
        recommended.recommended = Some(true);
    }
    plans
}

/// Returns every remainder amount whose exact count is needed to evaluate split plans.
pub(crate) fn split_remainder_amounts(
    target_amount_zat: u64,
    original_count_30d: u64,
    candidates: &[SplitCandidateCount],
) -> Vec<u64> {
    let original_score = compute_blend_score(original_count_30d);
    let scored_denominations = candidates
        .iter()
        .map(|candidate| ScoredDenomination {
            amount_zat: candidate.amount_zat,
            count_30d: candidate.count_30d,
            blend_score: compute_blend_score(candidate.count_30d),
        })
        .filter(|candidate| candidate.blend_score > original_score)
        .collect::<Vec<_>>();
    let mut by_amount = scored_denominations.clone();
    by_amount.sort_by_key(|candidate| Reverse(candidate.amount_zat));
    let mut by_score = scored_denominations;
    by_score.sort_by_key(|candidate| (Reverse(candidate.blend_score), candidate.amount_zat));
    let max_pieces = usize::try_from(
        target_amount_zat
            .div_ceil(MINIMUM_SPLIT_PIECE_ZAT)
            .min(MAX_SPLIT_PIECES as u64),
    )
    .unwrap_or(MAX_SPLIT_PIECES);
    let mut remainders = HashSet::new();
    for denominations in [&by_amount, &by_score] {
        for piece_limit in 2..=max_pieces {
            remainders.extend(
                greedy_split(target_amount_zat, denominations, piece_limit)
                    .into_iter()
                    .filter(|piece| piece.is_remainder)
                    .map(|piece| piece.amount_zat),
            );
        }
    }
    let mut remainders = remainders.into_iter().collect::<Vec<_>>();
    remainders.sort_unstable();
    remainders
}

fn greedy_split(
    target_amount_zat: u64,
    denominations: &[ScoredDenomination],
    max_pieces: usize,
) -> Vec<GreedyPiece> {
    let mut remaining_zat = target_amount_zat;
    let mut pieces = Vec::new();

    for denomination in denominations {
        while denomination.amount_zat > 0
            && remaining_zat >= denomination.amount_zat
            && pieces.len() < max_pieces - 1
        {
            pieces.push(GreedyPiece {
                amount_zat: denomination.amount_zat,
                count_30d: denomination.count_30d,
                blend_score: denomination.blend_score,
                is_remainder: false,
            });
            remaining_zat -= denomination.amount_zat;
            if remaining_zat <= IGNORED_REMAINDER_ZAT {
                remaining_zat = 0;
                break;
            }
        }
        if pieces.len() >= max_pieces - 1 || remaining_zat == 0 {
            break;
        }
    }

    if remaining_zat > IGNORED_REMAINDER_ZAT {
        pieces.push(GreedyPiece {
            amount_zat: remaining_zat,
            count_30d: 0,
            blend_score: 0,
            is_remainder: true,
        });
    }
    pieces
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Cipherscan exposes ZEC amounts as JavaScript numbers"
)]
fn zec_from_zatoshis(amount_zat: u64) -> f64 {
    amount_zat as f64 / ZATOSHIS_PER_ZEC
}

fn round_decimal(amount: f64, decimal_places: i32) -> f64 {
    let scale = 10_f64.powi(decimal_places);
    (amount * scale).round() / scale
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn score_boundaries_and_labels_match_cipherscan() {
        let cases = [
            (0, 0, BlendLabel::StandsOut),
            (1, 10, BlendLabel::StandsOut),
            (5, 25, BlendLabel::StandsOut),
            (10, 40, BlendLabel::Moderate),
            (25, 50, BlendLabel::Moderate),
            (50, 65, BlendLabel::Moderate),
            (100, 75, BlendLabel::BlendsWell),
            (200, 85, BlendLabel::BlendsWell),
            (500, 95, BlendLabel::BlendsWell),
        ];

        for (count, score, label) in cases {
            assert_eq!(compute_blend_score(count), score);
            assert_eq!(blend_label(score), label);
        }
    }

    #[test]
    fn nearby_amounts_sort_by_score_then_distance_and_keep_stable_ties()
    -> Result<(), serde_json::Error> {
        let nearby = nearby_popular_amounts(
            100_000_000,
            [
                NearbyCandidateCount {
                    amount_zat: 50_000_000,
                    count_30d: 200,
                },
                NearbyCandidateCount {
                    amount_zat: 120_000_000,
                    count_30d: 50,
                },
                NearbyCandidateCount {
                    amount_zat: 80_000_000,
                    count_30d: 50,
                },
                NearbyCandidateCount {
                    amount_zat: 101_234_567,
                    count_30d: 1,
                },
                NearbyCandidateCount {
                    amount_zat: 99_000_000,
                    count_30d: 0,
                },
            ],
        );

        assert_eq!(nearby.len(), 4);
        assert_eq!(
            serde_json::to_value(&nearby)?,
            json!([
                { "amount": 0.5, "count": 200, "blendScore": 85 },
                { "amount": 1.2, "count": 50, "blendScore": 65 },
                { "amount": 0.8, "count": 50, "blendScore": 65 },
                { "amount": 1.0123, "count": 1, "blendScore": 10 },
            ])
        );
        Ok(())
    }

    #[test]
    fn split_plans_use_greedy_order_rescore_remainders_and_deduplicate() {
        let plans = build_split_plans(
            350_000_000,
            0,
            [
                SplitCandidateCount {
                    amount_zat: 200_000_000,
                    count_30d: 100,
                },
                SplitCandidateCount {
                    amount_zat: 100_000_000,
                    count_30d: 500,
                },
                SplitCandidateCount {
                    amount_zat: 50_000_000,
                    count_30d: 50,
                },
            ],
            |amount_zat| (amount_zat == 150_000_000).then_some(25),
        );

        // Cipherscan records signatures before rejecting weak remainders, so those
        // rejected signatures also suppress equivalent later strategies.
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].min_blend_score, 50);
        assert_eq!(plans[0].piece_count, 2);
        assert_eq!(plans[0].recommended, Some(true));
        assert!(plans.iter().skip(1).all(|plan| plan.recommended.is_none()));
        assert!(plans.iter().any(|plan| {
            plan.pieces
                .iter()
                .any(|piece| piece.is_remainder && piece.blend_score == 50)
        }));
    }

    #[test]
    fn remainder_at_or_below_one_hundred_zatoshis_is_discarded() {
        let plans = build_split_plans(
            100_000_100,
            0,
            [SplitCandidateCount {
                amount_zat: 50_000_000,
                count_30d: 500,
            }],
            |_| None,
        );

        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].piece_count, 2);
        assert!(plans[0].pieces.iter().all(|piece| !piece.is_remainder));
    }

    #[test]
    fn split_plan_count_is_capped_at_twelve_pieces() {
        let plans = build_split_plans(
            2_000_000_000,
            0,
            [SplitCandidateCount {
                amount_zat: 100_000_000,
                count_30d: 500,
            }],
            |amount_zat| (amount_zat % 100_000_000 == 0).then_some(500),
        );

        assert!(plans.iter().all(|plan| plan.piece_count <= 12));
        assert!(plans.iter().any(|plan| plan.piece_count == 12));
    }

    #[test]
    fn remainder_discovery_returns_unique_exact_count_inputs() {
        let candidates = [
            SplitCandidateCount {
                amount_zat: 200_000_000,
                count_30d: 100,
            },
            SplitCandidateCount {
                amount_zat: 100_000_000,
                count_30d: 500,
            },
        ];
        let remainders = split_remainder_amounts(350_000_000, 0, &candidates);
        assert_eq!(remainders, vec![50_000_000, 150_000_000, 250_000_000]);
    }

    #[test]
    fn json_fields_and_numeric_amounts_match_legacy_shapes() -> Result<(), serde_json::Error> {
        let nearby = nearby_popular_amounts(
            123_456_789,
            [NearbyCandidateCount {
                amount_zat: 123_456_789,
                count_30d: 10,
            }],
        );
        assert_eq!(
            serde_json::to_value(&nearby[0])?,
            json!({
                "amount": 1.2346,
                "count": 10,
                "blendScore": 40,
            })
        );

        let plans = build_split_plans(
            150_000_001,
            0,
            [SplitCandidateCount {
                amount_zat: 100_000_000,
                count_30d: 100,
            }],
            |amount_zat| (amount_zat == 50_000_001).then_some(50),
        );
        let json = serde_json::to_value(&plans)?;
        assert_eq!(json[0]["pieces"][1]["amount"], json!(0.500_000_01));
        assert_eq!(json[0]["avgBlendScore"], json!(72));
        assert_eq!(json[0]["recommended"], json!(true));
        assert_eq!(json[0]["overallLabel"], json!("Moderate"));
        assert_eq!(json.get(1), None::<&Value>);
        Ok(())
    }
}
