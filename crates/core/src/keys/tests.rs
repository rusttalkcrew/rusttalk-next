use super::*;

#[test]
fn envelopes_bind_context_and_both_devices() {
    let (_, sender) = DeviceIdentity::create("Sender").unwrap();
    let (_, recipient) = DeviceIdentity::create("Recipient").unwrap();
    let (_, other) = DeviceIdentity::create("Other").unwrap();
    let scope = Scope {
        kind: Kind::Channel,
        id: [4; 32],
    };
    let key = random_key().unwrap();
    let context = EnvelopeContext::new([3; 32], scope, 1, &key);
    let bytes = context.wrap(&key, &recipient.certificate, &sender).unwrap();
    let open = |context: EnvelopeContext, bytes: &[u8]| {
        context.unwrap(&recipient, &sender.certificate, bytes)
    };
    assert!(open(context, &bytes).unwrap() == key);
    assert!(context.unwrap(&other, &sender.certificate, &bytes).is_err());
    assert!(
        context
            .unwrap(&recipient, &other.certificate, &bytes)
            .is_err()
    );
    assert!(
        open(
            EnvelopeContext {
                commitment: [0; 32],
                ..context
            },
            &bytes
        )
        .is_err()
    );
    let mut wrong_agreement = DeviceIdentity::from_key_bytes(
        recipient.signing_key.as_bytes(),
        recipient.agreement_key.as_bytes(),
        &recipient.sync_key,
        DeviceCertificate::verify_public(
            *recipient.certificate.person_id(),
            recipient.certificate.public_bytes().unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    wrong_agreement.agreement_key = other.agreement_key;
    assert!(matches!(
        context.unwrap(&wrong_agreement, &sender.certificate, &bytes),
        Err(KeyError::Authentication)
    ));
    for other in [
        EnvelopeContext {
            epoch: 2,
            ..context
        },
        EnvelopeContext {
            server_id: [5; 32],
            ..context
        },
        EnvelopeContext {
            scope: Scope {
                id: [5; 32],
                ..scope
            },
            ..context
        },
        EnvelopeContext {
            scope: Scope {
                kind: Kind::Space,
                ..scope
            },
            ..context
        },
    ] {
        let mut resigned = bytes[..80].to_vec();
        let info = other.info(recipient.certificate.signing_key().as_bytes());
        resigned.extend_from_slice(
            &sender
                .signing_key
                .sign(&[&info[..], &context.commitment, &resigned].concat())
                .to_bytes(),
        );
        assert!(open(other, &bytes).is_err());
        assert!(matches!(
            open(other, &resigned),
            Err(KeyError::Authentication)
        ));
    }
    for index in [32, 143] {
        let mut tampered = bytes.clone();
        tampered[index] ^= 1;
        assert!(
            context
                .verify(
                    wrong_agreement.certificate.signing_key().as_bytes(),
                    &sender.certificate,
                    &tampered
                )
                .is_err()
        );
    }
    assert!(
        context
            .verify(
                wrong_agreement.certificate.signing_key().as_bytes(),
                &sender.certificate,
                &bytes[..143]
            )
            .is_err()
    );
    let mut oversized = bytes;
    oversized.push(0);
    assert!(
        context
            .verify(
                wrong_agreement.certificate.signing_key().as_bytes(),
                &sender.certificate,
                &oversized
            )
            .is_err()
    );
}

#[test]
fn signed_envelope_must_open_to_the_committed_key() {
    let (_, device) = DeviceIdentity::create("Device").unwrap();
    let server = [3; 32];
    let scope = Scope {
        kind: Kind::Channel,
        id: [4; 32],
    };
    let context = EnvelopeContext::new(server, scope, 1, &Key::new([5; 32]));
    let wrong = Key::new([6; 32]);
    let bytes = context.wrap(&wrong, &device.certificate, &device).unwrap();
    assert!(
        context
            .verify(
                device.certificate.signing_key().as_bytes(),
                &device.certificate,
                &bytes
            )
            .is_ok()
    );
    assert!(matches!(
        context.unwrap(&device, &device.certificate, &bytes),
        Err(KeyError::Commitment)
    ));
}

#[test]
fn metadata_binds_object_epoch_and_key() {
    let key = random_key().unwrap();
    let id = b"channel";
    let server = [3; 32];
    let kind = Metadata::ChannelName;
    let mut ct = kind.seal(&server, id, 1, &key, b"secret").unwrap();
    assert_eq!(kind.open(&server, id, &key, &ct).unwrap(), b"secret");
    assert!(kind.open(&[4; 32], id, &key, &ct).is_err());
    assert!(kind.open(&server, b"other", &key, &ct).is_err());
    assert!(Metadata::ChannelTopic.open(&server, id, &key, &ct).is_err());
    assert!(kind.open(&server, id, &random_key().unwrap(), &ct).is_err());
    ct[7] = 2;
    assert!(kind.open(&server, id, &key, &ct).is_err());
    ct[7] = 1;
    ct[40] ^= 1;
    assert!(kind.open(&server, id, &key, &ct).is_err());
    assert!(
        kind.seal(&server, id, 1, &key, &[0; SMALL_METADATA + 1])
            .is_err()
    );
    assert!(kind.open(&server, id, &key, &ct[..39]).is_err());
}

#[test]
fn invite_key_binds_server_and_hides_epoch() {
    let space = random_key().unwrap();
    let link = random_key().unwrap();
    let server = [3; 32];
    let ct = seal_invite(&server, 2, &space, &link).unwrap();
    assert!(open_invite(&server, &link, &ct).unwrap() == (2, space));
    assert!(open_invite(&[4; 32], &link, &ct).is_err());
    assert!(open_invite(&server, &random_key().unwrap(), &ct).is_err());
    assert!(open_invite(&server, &link, &ct[..INVITE_BYTES - 1]).is_err());
}

#[test]
fn image_plaintext_limit_matches_ciphertext_limit() {
    let key = random_key().unwrap();
    let largest = vec![7; IMAGE_PLAINTEXT];
    let ct = Metadata::Avatar
        .seal(&[1; 32], b"person", 1, &key, &largest)
        .unwrap();
    assert_eq!(ct.len(), IMAGE_CIPHERTEXT);
    assert!(
        Metadata::Avatar
            .seal(&[1; 32], b"person", 1, &key, &[7; IMAGE_PLAINTEXT + 1])
            .is_err()
    );
}

#[test]
fn dropping_an_exclusion_or_changing_the_dm_pair_invalidates_the_commitment() {
    let key = random_key().unwrap();
    let scope = Scope {
        kind: Kind::Channel,
        id: [3; 32],
    };
    let excluded = Excluded::from([
        exclusion_id(&[9; 32], &[4; 32]),
        exclusion_id(&[9; 32], &[5; 32]),
    ]);
    let plain = EnvelopeContext::new([1; 32], scope, 2, &key);
    let dm = EnvelopeContext::new_dm([1; 32], scope, 2, &key, [[6; 32], [7; 32]]);
    let bound = dm.excluding(&excluded);
    assert_eq!(
        plain.excluding(&Default::default()).commitment,
        plain.commitment
    );
    assert_ne!(bound.commitment, plain.excluding(&excluded).commitment);
    assert_ne!(
        bound.commitment,
        dm.excluding(&Excluded::from([exclusion_id(&[9; 32], &[4; 32])]))
            .commitment
    );
    let (words, sender) = DeviceIdentity::create("sender").unwrap();
    let receiver = DeviceIdentity::add_device(&words, "receiver").unwrap();
    let wrap = bound.wrap(&key, &receiver.certificate, &sender).unwrap();
    assert!(dm.open(&receiver, &sender.certificate, &wrap).is_err());
    assert!(bound.open(&receiver, &sender.certificate, &wrap).unwrap() == key);
}

#[test]
fn message_created_events_with_and_without_a_record_parse() {
    // From servers up to v0.3.0, and for records too large to send inline.
    let old: Event =
        serde_json::from_str(r#"{"type":"message_created","channel":"c","id":"m"}"#).unwrap();
    assert!(matches!(old, Event::MessageCreated { record: None, .. }));
    let record = crate::messages::MessageRecord {
        backfilled: false,
        id: [1; 32],
        epoch: 2,
        person: [3; 32],
        sender_device: [4; 32],
        at: 5,
        size: 6,
        envelope: "sealed".into(),
    };
    let json = serde_json::to_string(&Event::MessageCreated {
        channel: "c".into(),
        id: "m".into(),
        record: Some(Box::new(record)),
    })
    .unwrap();
    let Event::MessageCreated {
        record: Some(back), ..
    } = serde_json::from_str(&json).unwrap()
    else {
        panic!("record lost: {json}");
    };
    assert!(back.id == [1; 32] && back.envelope == "sealed");
    // Clients up to v0.3.0 know only `channel` and `id` and skip the rest.
    #[derive(serde::Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum Old {
        MessageCreated { channel: String, id: String },
    }
    let Old::MessageCreated { channel, id } = serde_json::from_str(&json).unwrap();
    assert!(channel == "c" && id == "m");
}
