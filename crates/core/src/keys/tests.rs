use super::*;

#[test]
fn envelopes_bind_context_and_both_devices() {
    let (_, sender) = DeviceIdentity::create("Sender").unwrap();
    let (_, recipient) = DeviceIdentity::create("Recipient").unwrap();
    let (_, other) = DeviceIdentity::create("Other").unwrap();
    let scope = Scope {
        kind: Kind::Space,
        id: [4; 32],
    };
    let key = random_key().unwrap();
    let bytes = wrap(scope, 1, &key, &recipient.certificate, &sender).unwrap();
    let open =
        |scope, epoch, bytes: &[u8]| unwrap(scope, epoch, &recipient, &sender.certificate, bytes);
    assert!(open(scope, 1, &bytes).unwrap() == key);
    assert!(unwrap(scope, 1, &other, &sender.certificate, &bytes).is_err());
    assert!(unwrap(scope, 1, &recipient, &other.certificate, &bytes).is_err());
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
        unwrap(scope, 1, &wrong_agreement, &sender.certificate, &bytes),
        Err(KeyError::Authentication)
    ));
    for (other_scope, epoch) in [
        (scope, 2),
        (
            Scope {
                id: [5; 32],
                ..scope
            },
            1,
        ),
        (
            Scope {
                kind: Kind::Channel,
                ..scope
            },
            1,
        ),
    ] {
        let mut resigned = bytes[..80].to_vec();
        let info = envelope_info(
            other_scope,
            epoch,
            wrong_agreement.certificate.signing_key().as_bytes(),
        );
        resigned.extend_from_slice(
            &sender
                .signing_key
                .sign(&[&info[..], &resigned].concat())
                .to_bytes(),
        );
        assert!(open(other_scope, epoch, &bytes).is_err());
        assert!(matches!(
            open(other_scope, epoch, &resigned),
            Err(KeyError::Authentication)
        ));
    }
    for index in [32, 143] {
        let mut tampered = bytes.clone();
        tampered[index] ^= 1;
        assert!(
            verify_envelope(
                scope,
                1,
                wrong_agreement.certificate.signing_key().as_bytes(),
                &sender.certificate,
                &tampered
            )
            .is_err()
        );
    }
    assert!(
        verify_envelope(
            scope,
            1,
            wrong_agreement.certificate.signing_key().as_bytes(),
            &sender.certificate,
            &bytes[..143]
        )
        .is_err()
    );
    let mut oversized = bytes;
    oversized.push(0);
    assert!(
        verify_envelope(
            scope,
            1,
            wrong_agreement.certificate.signing_key().as_bytes(),
            &sender.certificate,
            &oversized
        )
        .is_err()
    );
}

#[test]
fn metadata_binds_object_epoch_and_key() {
    let key = random_key().unwrap();
    let id = b"channel";
    let kind = Metadata::ChannelName;
    let mut ct = kind.seal(id, 1, &key, b"secret").unwrap();
    assert_eq!(kind.open(id, &key, &ct).unwrap(), b"secret");
    assert!(kind.open(b"other", &key, &ct).is_err());
    assert!(Metadata::ChannelTopic.open(id, &key, &ct).is_err());
    assert!(kind.open(id, &random_key().unwrap(), &ct).is_err());
    ct[7] = 2;
    assert!(kind.open(id, &key, &ct).is_err());
    ct[7] = 1;
    ct[40] ^= 1;
    assert!(kind.open(id, &key, &ct).is_err());
    assert!(kind.seal(id, 1, &key, &[0; SMALL_METADATA + 1]).is_err());
    assert!(kind.open(id, &key, &ct[..39]).is_err());
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
