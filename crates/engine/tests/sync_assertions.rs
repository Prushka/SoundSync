//! The streaming assertions must distinguish a measured device gap from loss or mistimed audio.

mod common;

use std::slice::from_ref;

use common::{MS, assert_pulses_on_time};

fn pulses() -> Vec<i64> {
    (1..=10).map(|i| i * 200 * MS).collect()
}

#[test]
fn a_recorded_device_gap_excuses_only_the_pulse_it_erased() {
    let pulses = pulses();
    let onsets: Vec<_> = pulses.iter().enumerate().filter(|(i, _)| *i != 4).map(|(_, p)| p + 50 * MS).collect();
    assert_pulses_on_time("speaker", &pulses, &onsets, &[50 * MS], 0, from_ref(&(1040 * MS..1060 * MS)), MS, false);
}

#[test]
#[should_panic(expected = "pulses missing outside device gaps")]
fn an_unrelated_gap_does_not_hide_a_lost_packet() {
    let pulses = pulses();
    let onsets: Vec<_> = pulses.iter().skip(1).map(|p| p + 50 * MS).collect();
    assert_pulses_on_time("speaker", &pulses, &onsets, &[50 * MS], 0, from_ref(&(1040 * MS..1060 * MS)), MS, false);
}

#[test]
#[should_panic(expected = "played 2000 us off")]
fn device_gaps_do_not_relax_the_timing_tolerance() {
    let pulses = pulses();
    let onsets: Vec<_> = pulses.iter().map(|p| p + 52 * MS).collect();
    assert_pulses_on_time("speaker", &pulses, &onsets, &[50 * MS], 0, from_ref(&(0..3000 * MS)), MS, false);
}

#[test]
#[should_panic(expected = "only 0 of 10 pulses verified")]
fn a_starved_test_cannot_pass_without_enough_audio() {
    assert_pulses_on_time("speaker", &pulses(), &[], &[50 * MS], 0, from_ref(&(0..3000 * MS)), MS, false);
}
