//! `docs/proxy-behavior.md` §6.2 and §6.3 — estimating, and the comparison
//! that settled which estimator ships.
//!
//! **What is measured here is the mechanism, not the accuracy.** Real accuracy
//! is a comparison against counts only a live backend produces, and that is
//! roadmap §L. What can be settled offline is whether calibration converges,
//! and whether it converges on a systematic offset that exactness alone cannot
//! close. Both of those are properties of the estimators, not of the backend.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use proxenos::estimate::CalibratedEstimator;
use proxenos::estimate::Estimator;
use proxenos::estimate::shape_of;
use proxenos_core::anthropic::MessagesRequest;
use serde_json::json;

fn request(turns: usize, words_per_turn: usize) -> MessagesRequest {
    let body = vec!["consideration"; words_per_turn].join(" ");
    let messages: Vec<_> = (0..turns)
        .map(|index| {
            json!({
                "role": if index % 2 == 0 { "user" } else { "assistant" },
                "content": format!("{body} turn {index}"),
            })
        })
        .collect();

    serde_json::from_value(json!({
        "model": "claude-sonnet-5",
        "system": "You are Claude Code. ".repeat(40),
        "messages": messages,
        "tools": [{
            "name": "Read",
            "description": "Read a file from disk",
            "input_schema": {
                "type": "object",
                "properties": { "file_path": { "type": "string" } },
                "required": ["file_path"],
            },
        }],
    }))
    .unwrap()
}

/// A stand-in for what upstream charges.
///
/// It is the text cost plus a per-item framing cost the proxy cannot see. The
/// *shape* of this — a systematic overhead proportional to structure — is the
/// claim §6.3 rests on. Its magnitude is assumed, and that assumption is what
/// keeps this a measurement of the mechanism rather than of the backend.
fn modelled_upstream_count(request: &MessagesRequest) -> u64 {
    let shape = shape_of(request);
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let text = (shape.characters as f64 / 3.9).ceil() as u64;
    // Framing the proxy does not model: item envelopes, the serialized tool
    // schema as the backend renders it, the instructions wrapper.
    text.saturating_add(shape.items.saturating_mul(11))
        .saturating_add(220)
}

fn absolute_percentage_error(estimate: u64, truth: u64) -> f64 {
    if truth == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let error = (estimate as f64 - truth as f64).abs() / truth as f64;
    error * 100.0
}

/// §6.2 — an estimate is never zero. The client renders `message_start` live,
/// so a zero collapses the context meter at the start of every turn.
#[test]
fn an_estimate_is_never_zero_for_a_request_with_content() {
    let estimator = CalibratedEstimator::new();
    assert!(estimator.estimate(&request(1, 5)) > 0);
}

/// §6.3 — calibration corrects itself against upstream, and measurably so.
#[test]
fn calibration_reduces_the_error_across_a_session() {
    let estimator = CalibratedEstimator::new();

    let mut first_error = None;
    let mut last_error = 0.0;

    // A session that grows, as a real one does.
    for turn in 1..=12 {
        let request = request(turn * 2, 60);
        let truth = modelled_upstream_count(&request);
        let estimate = estimator.estimate(&request);

        let error = absolute_percentage_error(estimate, truth);
        if first_error.is_none() {
            first_error = Some(error);
        }
        last_error = error;

        // Ground truth arrives within the same exchange, and is fed back.
        estimator.observe(estimate, truth);
    }

    let first_error = first_error.unwrap();
    assert!(
        last_error < first_error,
        "calibration did not improve the estimate: {first_error:.1}% then {last_error:.1}%"
    );
    // Measured: 20.3% on the first turn, under 1% from the third onward. The
    // threshold is set where the measurement lands, not where it would be
    // convenient.
    assert!(
        last_error < 1.0,
        "after a dozen turns the estimate is still {last_error:.1}% out"
    );
    assert!(estimator.observations() >= 12);
}

/// The fit needs two differently sized observations before it can separate
/// scale from offset. Until then it falls back to a plain ratio rather than
/// solving a line through one point.
#[test]
fn one_observation_is_not_enough_to_fit_a_line() {
    let estimator = CalibratedEstimator::new();
    let small = request(2, 20);

    let estimate = estimator.estimate(&small);
    estimator.observe(estimate, estimate.saturating_mul(2));

    // It has learned something — the ratio moved — without pretending to know
    // a slope.
    assert!(estimator.ratio() > 1.5);
    assert_eq!(estimator.observations(), 1);
}

/// Observations all at one size say nothing about how cost scales, so no slope
/// is invented from them.
#[test]
fn observations_at_a_single_size_do_not_produce_a_slope() {
    let estimator = CalibratedEstimator::new();
    let fixed = request(4, 30);

    for _ in 0..5 {
        let estimate = estimator.estimate(&fixed);
        estimator.observe(estimate, 4_000);
    }

    // It converges on the right answer for that size without extrapolating.
    let estimate = estimator.estimate(&fixed);
    let error = absolute_percentage_error(estimate, 4_000);
    assert!(error < 5.0, "{error:.1}% off at the size it was taught");
}

/// Before a session's first completed request the estimate is uncalibrated, and
/// the ratio says so rather than pretending otherwise.
#[test]
fn an_uncalibrated_estimator_reports_no_observations() {
    let estimator = CalibratedEstimator::new();

    assert_eq!(estimator.observations(), 0);
    assert!((estimator.ratio() - 1.0).abs() < f64::EPSILON);
}

/// One unusual turn nudges the fit rather than redefining it. A single
/// anomalous count would otherwise throw every later estimate in the session.
#[test]
fn a_single_outlier_does_not_capture_the_fit() {
    let estimator = CalibratedEstimator::new();

    // A settled session across a range of sizes.
    for turn in 1..=10 {
        let request = request(turn, 40);
        let truth = modelled_upstream_count(&request);
        let estimate = estimator.estimate(&request);
        estimator.observe(estimate, truth);
    }

    let probe = request(6, 40);
    let before = estimator.estimate(&probe);

    // One turn where upstream reports something absurd.
    estimator.observe(before, before.saturating_mul(9));
    let after = estimator.estimate(&probe);

    #[allow(clippy::cast_precision_loss)]
    let moved = (after as f64 - before as f64).abs() / before as f64;
    assert!(
        moved < 1.0,
        "one outlier moved the estimate by {:.0}%",
        moved * 100.0
    );
}

/// A zero on either side teaches nothing and must not poison the ratio.
#[test]
fn degenerate_observations_are_ignored() {
    let estimator = CalibratedEstimator::new();
    estimator.observe(0, 500);
    estimator.observe(500, 0);

    assert_eq!(estimator.observations(), 0);
    assert!((estimator.ratio() - 1.0).abs() < f64::EPSILON);
}

/// §6.3 — the comparison, and the reason the calibrated estimator ships.
///
/// The tokenizer counts text exactly. The figure that matters is not the text
/// count: it includes framing the proxy does not model identically. Exactness
/// over structurally different input is authoritatively wrong, which is worse
/// than approximate and self-correcting — and this measures that difference
/// rather than asserting it.
#[cfg(feature = "tokenizer")]
#[test]
fn the_calibrated_estimator_beats_the_tokenizer_once_calibrated() {
    use proxenos::estimate::TokenizerEstimator;

    let calibrated = CalibratedEstimator::new();
    let tokenizer = TokenizerEstimator::new().expect("the tokenizer should load");

    let mut calibrated_errors = Vec::new();
    let mut tokenizer_errors = Vec::new();

    for turn in 1..=12 {
        let request = request(turn * 2, 60);
        let truth = modelled_upstream_count(&request);

        let from_calibrated = calibrated.estimate(&request);
        let from_tokenizer = tokenizer.estimate(&request);

        // Only the second half is scored. The first is the calibration period,
        // and scoring it would measure how fast each converges rather than
        // where each lands.
        if turn > 6 {
            calibrated_errors.push(absolute_percentage_error(from_calibrated, truth));
            tokenizer_errors.push(absolute_percentage_error(from_tokenizer, truth));
        }

        calibrated.observe(from_calibrated, truth);
        tokenizer.observe(from_tokenizer, truth);
    }

    let mean = |errors: &[f64]| errors.iter().sum::<f64>() / errors.len() as f64;
    let calibrated_error = mean(&calibrated_errors);
    let tokenizer_error = mean(&tokenizer_errors);

    println!("calibrated: {calibrated_error:.2}%  tokenizer: {tokenizer_error:.2}%");

    assert!(
        calibrated_error < tokenizer_error,
        "the tokenizer was closer ({tokenizer_error:.2}% against {calibrated_error:.2}%), \
         which contradicts §6.3 and means the shipped default is wrong"
    );
}

/// The tokenizer has nothing to learn from a correction, and says so by not
/// changing. It is not a defect — it is the property being compared.
#[cfg(feature = "tokenizer")]
#[test]
fn the_tokenizer_does_not_calibrate() {
    use proxenos::estimate::TokenizerEstimator;

    let tokenizer = TokenizerEstimator::new().unwrap();
    let request = request(4, 40);

    let before = tokenizer.estimate(&request);
    tokenizer.observe(before, before.saturating_mul(3));
    let after = tokenizer.estimate(&request);

    assert_eq!(before, after);
}
