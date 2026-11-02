//! Update management remains readable when the audio protocol changes.

use soundsync_core::protocol::{DecodeError, GroupKey, MAX_DATAGRAM};
use soundsync_core::remote_update::{self, UpdateAction, UpdateInfo, UpdateMessage, UpdateState};
use soundsync_core::Os;

fn info() -> UpdateInfo {
    UpdateInfo { name: "MacBook".into(), os: Os::MacOs, version: "0.1.0 (build 20)".into(), build: Some(20), protocol: 200, state: UpdateState::Available { build: 21 } }
}

#[test]
fn update_messages_work_independently_of_the_audio_protocol() {
    let key = GroupKey::derive("home", "secret");
    let messages = [
        UpdateMessage::Status { reply: false, info: info() },
        UpdateMessage::Request { target: 42, session: 99, request: 1, action: UpdateAction::Install { build: 21 } },
        UpdateMessage::Request { target: 42, session: 99, request: 2, action: UpdateAction::Check },
        UpdateMessage::Ack { target: 42, session: 99, request: 1, error: None },
        UpdateMessage::Result { target: 42, session: 99, request: 1, state: UpdateState::Failed { error: "read-only folder".into(), build: Some(21) } },
        UpdateMessage::Result { target: 42, session: 99, request: 2, state: UpdateState::UpToDate },
        UpdateMessage::Result { target: 42, session: 99, request: 3, state: UpdateState::Restarting { build: 21 } },
    ];
    for message in messages {
        let mut bytes = [0; MAX_DATAGRAM];
        let n = remote_update::encode(7, 123, &message, &key, &mut bytes).unwrap();
        assert_eq!(&bytes[..4], b"SSUP");
        assert_eq!(bytes[4], 1, "management version is independent of the audio protocol");
        assert_eq!(remote_update::decode(&bytes[..n], &key).unwrap(), (7, 123, message));
        assert_eq!(soundsync_core::protocol::decode(&bytes[..n], &key).unwrap_err(), DecodeError::BadMagic);
    }
}

#[test]
fn updates_require_the_group_key_and_reject_damaged_or_oversized_packets() {
    let key = GroupKey::derive("home", "secret");
    let mut bytes = [0; MAX_DATAGRAM];
    let n = remote_update::encode(7, 123, &UpdateMessage::Status { reply: true, info: info() }, &key, &mut bytes).unwrap();
    assert_eq!(remote_update::decode(&bytes[..n], &GroupKey::derive("home", "wrong")).unwrap_err(), DecodeError::BadTag);
    bytes[n - 1] ^= 1;
    assert_eq!(remote_update::decode(&bytes[..n], &key).unwrap_err(), DecodeError::BadTag);
    assert!(remote_update::decode(&bytes[..10], &key).is_err());
    assert!(remote_update::decode(&vec![0; MAX_DATAGRAM + 1], &key).is_err());
    let mut invalid = info();
    invalid.state = UpdateState::Downloading { build: 21, received: 100, total: 10 };
    assert!(remote_update::encode(7, 123, &UpdateMessage::Status { reply: true, info: invalid }, &key, &mut bytes).is_err());
    for state in [UpdateState::Checking, UpdateState::Downloading { build: 21, received: 1, total: 10 },
        UpdateState::Failed { error: "x".repeat(remote_update::MAX_TEXT + 1), build: Some(21) }]
    {
        assert!(remote_update::encode(7, 123, &UpdateMessage::Result { target: 42, session: 99, request: 1, state }, &key, &mut bytes).is_err());
    }
    assert!(remote_update::encode(7, 123, &UpdateMessage::Result { target: 42, session: 99, request: 0, state: UpdateState::UpToDate }, &key, &mut bytes).is_err());
}

#[test]
fn version_one_keeps_its_original_wire_layout() {
    let key = GroupKey::derive("home", "secret");
    let mut buf = [0; MAX_DATAGRAM];
    let cases = [
        (UpdateMessage::Status { reply: false, info: UpdateInfo {
            name: "M".into(), os: Os::Windows, version: "v".into(), build: Some(20), protocol: 200, state: UpdateState::Available { build: 21 },
        } }, vec![0, 0, 1, b'M', 0, 1, b'v', 1, 20, 200, 4, 21]),
        (UpdateMessage::Request { target: 42, session: 99, request: 1, action: UpdateAction::Install { build: 21 } }, vec![1, 42, 99, 1, 1, 21]),
        (UpdateMessage::Ack { target: 42, session: 99, request: 1, error: None }, vec![2, 42, 99, 1, 0]),
        (UpdateMessage::Result { target: 42, session: 99, request: 1, state: UpdateState::UpToDate }, vec![3, 42, 99, 1, 3]),
    ];
    for (message, body) in cases {
        let n = remote_update::encode(7, 123, &message, &key, &mut buf).unwrap();
        assert_eq!(&buf[..5], b"SSUP\x01");
        assert_eq!(&buf[5..13], &7u64.to_le_bytes());
        assert_eq!(&buf[13..21], &123u64.to_le_bytes());
        assert_eq!(&buf[21..n - 8], &body, "changing this layout would strand devices running the first remote-update release");
    }
}
