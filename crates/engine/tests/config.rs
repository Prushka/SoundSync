mod common;

use std::time::Duration;

use common::*;
use soundsync_audio::fake::FakeClock;
use soundsync_core::config::Config;
use soundsync_engine::Command;

#[test]
fn engine_fingerprints_the_config_file_it_owns() {
    // The UI compares the file with this to tell the engine's own saves from hand edits.
    let n = spawn("n", FakeClock::default(), |h| h.add_output(output("TV", 0.0, 10, 0)));
    let path = n.config_path();
    let on_disk = || Config::fingerprint(&std::fs::read(&path).unwrap());
    assert_eq!(n.engine.config_fingerprint(), on_disk());
    n.engine.command(Command::SetVolume(0.25));
    assert!(wait_until(Duration::from_secs(1), || std::fs::read_to_string(&path).unwrap().contains("volume = 0.25")));
    assert_eq!(n.engine.config_fingerprint(), on_disk(), "the engine's own save is not an edit");
    n.shutdown();
}
