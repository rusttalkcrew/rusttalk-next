use super::*;

fn known_words() -> String {
    "abandon ".repeat(23) + "art"
}

fn other_words() -> String {
    Mnemonic::from_entropy(&[0xff; 32]).unwrap().to_string()
}

#[test]
fn known_words_derive_the_same_person_and_sync_key_on_fresh_devices() {
    let words = known_words();
    let first = DeviceIdentity::add_device(&words, "First").unwrap();
    let second = DeviceIdentity::add_device(&words, "Second").unwrap();
    let other = DeviceIdentity::add_device(&other_words(), "Other").unwrap();
    let person = first.certificate.person_id();

    assert_eq!(
        *person,
        [
            0x90, 0xf9, 0xc9, 0x57, 0x5d, 0xdc, 0xdc, 0x65, 0xb5, 0xe9, 0x01, 0xc9, 0xe4, 0x58,
            0x6e, 0xe3, 0xa1, 0x63, 0x8b, 0x51, 0xc8, 0xfd, 0x67, 0x6d, 0x5c, 0xc7, 0xab, 0xc9,
            0x3c, 0x8a, 0xac, 0x9d,
        ]
    );
    assert!(
        *first.sync_key
            == [
                0x3b, 0x58, 0xb4, 0x22, 0x5e, 0x8b, 0x9c, 0x2e, 0x1b, 0x2e, 0xca, 0x33, 0x79, 0x36,
                0x1a, 0xe4, 0x09, 0x9e, 0xa5, 0x5f, 0x26, 0xfd, 0x55, 0x34, 0xb7, 0x6f, 0x54, 0x79,
                0xff, 0x73, 0xdc, 0x13,
            ]
    );
    assert_eq!(fingerprint(person), "f851-b917-0ea8-a575");
    assert_eq!(person, second.certificate.person_id());
    assert_ne!(person, other.certificate.person_id());
    assert!(first.sync_key == second.sync_key);
    assert!(first.sync_key != other.sync_key);
    assert_ne!(
        first.certificate.signing_key(),
        second.certificate.signing_key()
    );
    assert_ne!(
        first.certificate.agreement_key(),
        second.certificate.agreement_key()
    );
}

#[test]
fn generated_words_restore_and_bad_recovery_phrases_fail() {
    let (words, identity) = DeviceIdentity::create("Desktop").unwrap();
    assert_eq!(words.split_whitespace().count(), 24);
    let restored = DeviceIdentity::add_device(&words, "New Desktop").unwrap();
    assert_eq!(
        identity.certificate.person_id(),
        restored.certificate.person_id()
    );
    assert!(identity.sync_key == restored.sync_key);

    for words in ["abandon ".repeat(11) + "about", "abandon ".repeat(24)] {
        assert!(matches!(
            DeviceIdentity::add_device(&words, "Desktop"),
            Err(IdentityError::InvalidWords)
        ));
    }
}

#[test]
fn certificate_round_trips_and_rejects_untrusted_input() {
    let name = "é".repeat(32);
    let identity = DeviceIdentity::add_device(&known_words(), &name).unwrap();
    let cert = &identity.certificate;
    let person = *cert.person_id();
    let bytes = cert.to_bytes();
    let decoded = DeviceCertificate::verify(person, &bytes).unwrap();
    assert_eq!(decoded.to_bytes(), bytes);
    assert_eq!(decoded.name(), name);
    assert_eq!(decoded.created_at(), cert.created_at());
    assert_eq!(decoded.signing_key(), &identity.signing_key.verifying_key());
    assert_eq!(
        decoded.agreement_key(),
        AgreementPublicKey::from(&identity.agreement_key).as_bytes()
    );

    let wrong_person = derive_keys(&parse_words(&other_words()).unwrap())
        .0
        .verifying_key();
    assert!(matches!(
        DeviceCertificate::verify(wrong_person.to_bytes(), &bytes),
        Err(IdentityError::InvalidSignature)
    ));
    for i in [bytes.len() - 1, CERT_DOMAIN.len() + 64] {
        let mut tampered = bytes.clone();
        tampered[i] ^= 1;
        assert!(matches!(
            DeviceCertificate::verify(person, &tampered),
            Err(IdentityError::InvalidSignature)
        ));
    }

    for len in 0..bytes.len() {
        assert!(DeviceCertificate::verify(person, &bytes[..len]).is_err());
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(DeviceCertificate::verify(person, &trailing).is_err());
    assert!(DeviceCertificate::verify(person, &vec![0; bytes.len()]).is_err());

    let (root, _) = derive_keys(&parse_words(&known_words()).unwrap());
    let too_long = name + "x";
    assert!(matches!(
        DeviceIdentity::add_device(&known_words(), &too_long),
        Err(IdentityError::InvalidDeviceName)
    ));
    let mut oversized = certificate_payload(
        cert.signing_key(),
        cert.agreement_key(),
        cert.created_at(),
        &too_long,
    );
    oversized.extend_from_slice(&root.sign(&oversized).to_bytes());
    assert!(matches!(
        DeviceCertificate::verify(person, &oversized),
        Err(IdentityError::InvalidEncoding)
    ));

    let mut invalid_utf8 = bytes[..bytes.len() - 64].to_vec();
    *invalid_utf8.last_mut().unwrap() = 0xff;
    invalid_utf8.extend_from_slice(&root.sign(&invalid_utf8).to_bytes());
    assert!(matches!(
        DeviceCertificate::verify(person, &invalid_utf8),
        Err(IdentityError::InvalidEncoding)
    ));
}

#[test]
fn stored_keys_must_match_both_certificate_keys() {
    let identity = DeviceIdentity::add_device(&known_words(), "Desktop").unwrap();
    let person = *identity.certificate.person_id();
    let bytes = identity.certificate.to_bytes();
    let signing = identity.signing_key.as_bytes();
    let agreement = identity.agreement_key.as_bytes();
    let mut wrong_signing = *signing;
    wrong_signing[0] ^= 8;
    let mut wrong_agreement = *agreement;
    wrong_agreement[0] ^= 8;

    for (signing, agreement) in [(&wrong_signing, agreement), (signing, &wrong_agreement)] {
        let cert = DeviceCertificate::verify(person, &bytes).unwrap();
        assert!(matches!(
            DeviceIdentity::from_key_bytes(signing, agreement, &identity.sync_key, cert),
            Err(IdentityError::KeyMismatch)
        ));
    }
}

#[test]
fn revocation_requires_the_same_person_and_device() {
    let words = known_words();
    let first = DeviceIdentity::add_device(&words, "First")
        .unwrap()
        .certificate;
    let second = DeviceIdentity::add_device(&words, "Second")
        .unwrap()
        .certificate;
    let person = *first.person_id();
    let device = first.signing_key().to_bytes();
    let bytes = revoke_device(&words, device).unwrap().to_bytes();
    let revocation = Revocation::verify(person, &bytes).unwrap();
    assert_eq!(revocation.to_bytes(), bytes);
    assert!(!first.is_revoked(&[]));
    let revocations = [revocation];
    assert!(first.is_revoked(&revocations));
    assert!(!second.is_revoked(&revocations));

    let foreign = revoke_device(&other_words(), device).unwrap();
    let foreign_bytes = foreign.to_bytes();
    assert!(matches!(
        Revocation::verify(person, &foreign_bytes),
        Err(IdentityError::InvalidSignature)
    ));
    let foreign = Revocation::verify(foreign.person_id, &foreign_bytes).unwrap();
    assert!(!first.is_revoked(&[foreign]));

    assert!(Revocation::verify(person, &bytes[..bytes.len() - 1]).is_err());
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(Revocation::verify(person, &trailing).is_err());
    let mut tampered = bytes;
    tampered[REVOCATION_DOMAIN.len() + 32] ^= 1;
    assert!(matches!(
        Revocation::verify(person, &tampered),
        Err(IdentityError::InvalidSignature)
    ));
    assert!(Revocation::verify(person, &first.to_bytes()).is_err());
}

#[test]
fn confirmation_uses_three_distinct_indices_and_ignores_case_and_whitespace() {
    let phrase = known_words();
    let words: Vec<_> = phrase.split_whitespace().collect();
    let confirmation = WordConfirmation::random().unwrap();
    let [first, second, third] = confirmation.indices();
    assert!(first < second && second < third && third < 24);
    let answers = [words[first], words[second], words[third]];
    assert!(confirmation.matches(&phrase, answers));
    assert!(!confirmation.matches(&phrase, ["wrong", answers[1], answers[2]]));
    let normalized = answers.map(|word| format!("  {} \t\n", word.to_ascii_uppercase()));
    assert!(confirmation.matches(&phrase, normalized.each_ref().map(String::as_str)));
}
