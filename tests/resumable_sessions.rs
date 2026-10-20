//! Resumable sessions preserve output, not application-level exactly-once delivery.

use nulang::runtime::resumable_session::{
    SessionClientMessage, SessionCursor, SessionError, SessionIdentity, SessionProtocolVersion,
    SessionRegistry, SessionReplay, SessionServerMessage,
};

fn owner() -> SessionIdentity {
    SessionIdentity::new("tenant-a", "operator-1").unwrap()
}

fn registry() -> SessionRegistry {
    SessionRegistry::new(2, 16).unwrap()
}

#[test]
fn reconnect_replays_only_missing_output_and_fences_previous_connection() {
    let mut sessions = registry();
    let ready = sessions.open("opaque-session-1", owner()).unwrap();
    assert_eq!(ready.epoch, SessionCursor(1));
    assert_eq!(sessions.append_output("opaque-session-1", b"one".to_vec()).unwrap(), SessionCursor(1));
    assert_eq!(sessions.append_output("opaque-session-1", b"two".to_vec()).unwrap(), SessionCursor(2));

    let initial = sessions.read_after("opaque-session-1", &owner(), ready.epoch, SessionCursor(0)).unwrap();
    assert!(matches!(initial, SessionReplay::Replayed { .. }));
    sessions.ack("opaque-session-1", &owner(), ready.epoch, SessionCursor(1)).unwrap();

    let recovered = sessions.resume("opaque-session-1", &owner(), SessionCursor(1)).unwrap();
    match recovered {
        SessionReplay::Replayed { epoch, latest, frames } => {
            assert_eq!(epoch, SessionCursor(2));
            assert_eq!(latest, SessionCursor(2));
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].sequence, SessionCursor(2));
            assert_eq!(frames[0].bytes, b"two");
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
    assert_eq!(
        sessions.ack("opaque-session-1", &owner(), ready.epoch, SessionCursor(2)),
        Err(SessionError::StaleEpoch)
    );
    sessions.ack("opaque-session-1", &owner(), SessionCursor(2), SessionCursor(2)).unwrap();
}

#[test]
fn overwritten_history_returns_explicit_gap_instead_of_skipping_output() {
    let mut sessions = registry();
    sessions.open("s", owner()).unwrap();
    sessions.append_output("s", b"first".to_vec()).unwrap();
    sessions.append_output("s", b"second".to_vec()).unwrap();
    sessions.append_output("s", b"third".to_vec()).unwrap();

    let outcome = sessions.resume("s", &owner(), SessionCursor(0)).unwrap();
    assert_eq!(
        outcome,
        SessionReplay::Gap {
            epoch: SessionCursor(2),
            requested: SessionCursor(0),
            first_available: SessionCursor(2),
            latest: SessionCursor(3),
        }
    );
}

#[test]
fn owner_is_bound_to_tenant_and_principal_and_hidden_from_other_callers() {
    let mut sessions = registry();
    sessions.open("s", owner()).unwrap();
    sessions.append_output("s", b"secret".to_vec()).unwrap();

    let different_tenant = SessionIdentity::new("tenant-b", "operator-1").unwrap();
    let different_principal = SessionIdentity::new("tenant-a", "operator-2").unwrap();

    for identity in [&different_tenant, &different_principal] {
        assert_eq!(
            sessions.resume("s", identity, SessionCursor(0)),
            Err(SessionError::NotFound)
        );
        assert_eq!(
            sessions.read_after("s", identity, SessionCursor(1), SessionCursor(0)),
            Err(SessionError::NotFound)
        );
        assert_eq!(
            sessions.close("s", identity, SessionCursor(1)),
            Err(SessionError::NotFound)
        );
    }
    assert!(sessions.read_after("s", &owner(), SessionCursor(1), SessionCursor(0)).is_ok());
}

#[test]
fn cursor_and_ack_reject_unserved_or_future_sequences() {
    let mut sessions = registry();
    let ready = sessions.open("s", owner()).unwrap();
    sessions.append_output("s", b"one".to_vec()).unwrap();
    assert_eq!(
        sessions.resume("s", &owner(), SessionCursor(2)),
        Err(SessionError::CursorAhead)
    );
    assert_eq!(
        sessions.ack("s", &owner(), ready.epoch, SessionCursor(1)),
        Err(SessionError::CursorAhead)
    );
    sessions.read_after("s", &owner(), ready.epoch, SessionCursor(0)).unwrap();
    sessions.ack("s", &owner(), ready.epoch, SessionCursor(1)).unwrap();
    assert_eq!(
        sessions.ack("s", &owner(), ready.epoch, SessionCursor(0)),
        Err(SessionError::CursorRegression)
    );
}

#[test]
fn frame_size_is_bounded_without_mutating_sequence_or_history() {
    let mut sessions = registry();
    let ready = sessions.open("s", owner()).unwrap();
    assert_eq!(
        sessions.append_output("s", vec![42; 17]),
        Err(SessionError::FrameTooLarge)
    );
    assert_eq!(
        sessions.read_after("s", &owner(), ready.epoch, SessionCursor(0)).unwrap(),
        SessionReplay::Replayed {
            epoch: ready.epoch,
            latest: SessionCursor(0),
            frames: vec![],
        }
    );
}

#[test]
fn protocol_rejects_wrong_version_and_preserves_u64_cursors_in_json() {
    let message = SessionClientMessage::Resume {
        version: SessionProtocolVersion,
        session_id: "s".to_owned(),
        after: SessionCursor(u64::MAX),
    };
    let json = serde_json::to_string(&message).unwrap();
    assert!(json.contains(r#""18446744073709551615""#));
    assert_eq!(serde_json::from_str::<SessionClientMessage>(&json).unwrap(), message);
    assert!(serde_json::from_str::<SessionClientMessage>(
        r#"{"type":"resume","version":"nulang-session/2","session_id":"s","after":"0"}"#
    ).is_err());
    assert!(serde_json::from_str::<SessionCursor>("9007199254740993").is_err());
    assert!(serde_json::from_str::<SessionCursor>(r#""01""#).is_err());

    let output = SessionServerMessage::Output {
        session_id: "s".to_owned(),
        epoch: SessionCursor(4),
        sequence: SessionCursor(9),
        bytes: b"ok".to_vec(),
    };
    assert_eq!(
        serde_json::from_str::<SessionServerMessage>(&serde_json::to_string(&output).unwrap()).unwrap(),
        output
    );
}

#[test]
fn invalid_identity_and_configuration_are_rejected() {
    assert_eq!(SessionRegistry::new(0, 32), Err(SessionError::InvalidConfig));
    assert_eq!(SessionRegistry::new(2, 0), Err(SessionError::InvalidConfig));
    assert_eq!(SessionIdentity::new("", "p"), Err(SessionError::InvalidIdentity));
    assert_eq!(SessionIdentity::new("tenant", ""), Err(SessionError::InvalidIdentity));
    let mut sessions = registry();
    assert_eq!(sessions.open("", owner()), Err(SessionError::InvalidIdentity));
    sessions.open("s", owner()).unwrap();
    assert_eq!(sessions.open("s", owner()), Err(SessionError::AlreadyExists));
}

#[test]
fn close_requires_current_epoch_after_reconnect() {
    let mut sessions = registry();
    sessions.open("s", owner()).unwrap();
    sessions.resume("s", &owner(), SessionCursor(0)).unwrap();
    assert_eq!(sessions.close("s", &owner(), SessionCursor(1)), Err(SessionError::StaleEpoch));
    sessions.close("s", &owner(), SessionCursor(2)).unwrap();
    assert_eq!(sessions.resume("s", &owner(), SessionCursor(0)), Err(SessionError::NotFound));
}
