use ed25519_dalek::Signer;

use super::*;
use crate::{
    auth::encode,
    identity::DeviceIdentity,
    keys::Key,
    messages::{Attachment, BlobRef, MessageBody, MessageContext, MessageError},
};

const SERVER: [u8; 32] = [7; 32];
const REPORT: [u8; 32] = [8; 32];

fn person(name: &str) -> DeviceIdentity {
    DeviceIdentity::create(name).unwrap().1
}

fn context(sender: &DeviceIdentity, id: u8) -> MessageContext {
    MessageContext {
        server_id: SERVER,
        channel_id: [3; 32],
        epoch: 1,
        id: [id; 32],
        sender_device: sender.certificate.signing_key().to_bytes(),
        person: *sender.certificate.person_id(),
    }
}

fn text(text: &str) -> MessageBody {
    MessageBody {
        text: text.into(),
        ..Default::default()
    }
}

/// What the reporter's app builds from a message it opened with the channel key.
fn evidence(
    context: MessageContext,
    key: &Key,
    sender: &DeviceIdentity,
    envelope: Vec<u8>,
) -> Evidence {
    Evidence {
        context,
        sender: sender.certificate.clone(),
        message_key: context
            .disclose(key, &sender.certificate, &envelope)
            .unwrap(),
        envelope,
    }
}

fn report(evidence: Vec<Evidence>) -> Report {
    Report {
        reason: Reason::Harassment,
        note: "keeps doing this".into(),
        evidence,
    }
}

#[test]
fn reviewers_verify_the_reported_message_and_its_files() {
    let (sender, reporter) = (person("sender"), person("reporter"));
    let (moderator, phone, outsider) = (person("mod"), person("mod phone"), person("other"));
    let channel_key = Key::new([1; 32]);
    let file = Attachment {
        file: BlobRef {
            id: [5; 32],
            key: Key::new([6; 32]),
            size: 10,
            chunk_size: 65536,
        },
        name: "proof.png".into(),
        media_type: "image/png".into(),
        thumbnail: None,
    };
    let body = MessageBody {
        attachments: vec![file],
        ..text("you again")
    };
    let context = context(&sender, 4);
    let envelope = context.seal(&channel_key, &sender, &body).unwrap();
    let item = evidence(context, &channel_key, &sender, envelope);
    // The disclosed key is not the channel key, and opens nothing else in the epoch.
    assert_ne!(*item.message_key, *channel_key);
    let other = MessageContext {
        id: [9; 32],
        ..context
    };
    let other_envelope = other.seal(&channel_key, &sender, &text("other")).unwrap();
    assert_eq!(
        other
            .open_disclosed(&item.message_key, &sender.certificate, &other_envelope)
            .err(),
        Some(MessageError::Authentication)
    );
    let reviewers = [&moderator.certificate, &phone.certificate];
    let (sealed, wraps) = report(vec![item])
        .seal(&SERVER, &REPORT, false, &reporter, &reviewers)
        .unwrap();
    assert_eq!(wraps.len(), 2);
    assert!(wraps.iter().all(|(_, wrap)| wrap.len() == WRAP_BYTES));
    let reporter_id = *reporter.certificate.person_id();
    for (device, (key, wrap)) in [&moderator, &phone].into_iter().zip(&wraps) {
        assert_eq!(key, device.certificate.signing_key().as_bytes());
        let opened = open(&SERVER, &REPORT, &reporter_id, false, device, wrap, &sealed).unwrap();
        assert_eq!(opened.reason, Reason::Harassment);
        assert_eq!(opened.note, "keeps doing this");
        assert_eq!(
            &opened.reporter_device,
            reporter.certificate.signing_key().as_bytes()
        );
        let (shown, verified_sender, shown_body) = &opened.messages[0];
        assert_eq!(shown.id, [4; 32]);
        assert_eq!(verified_sender.person_id(), sender.certificate.person_id());
        // The file keys come with the message, and only its files.
        assert!(*shown_body == body);
        // A reviewer who holds the channel key confirms the disclosed key.
        assert_eq!(
            opened.confirms(|_| Some(channel_key.clone())),
            Confirmation::Matched
        );
        assert_eq!(
            opened.confirms(|_| Some(Key::new([2; 32]))),
            Confirmation::Mismatch
        );
        assert_eq!(opened.confirms(|_| None), Confirmation::Absent);
    }
    // Someone else's device, or a wrap moved to another device, opens nothing.
    for (device, wrap) in [(&outsider, &wraps[0].1), (&phone, &wraps[0].1)] {
        assert_eq!(
            open(&SERVER, &REPORT, &reporter_id, false, device, wrap, &sealed).err(),
            Some(ReportError::Authentication)
        );
    }
}

#[test]
fn forged_or_tampered_evidence_fails_verification() {
    let (sender, reporter, moderator) = (person("sender"), person("reporter"), person("mod"));
    let channel_key = Key::new([1; 32]);
    let context = context(&sender, 4);
    let envelope = context.seal(&channel_key, &sender, &text("hello")).unwrap();
    let genuine = || evidence(context, &channel_key, &sender, envelope.clone());
    let reporter_id = *reporter.certificate.person_id();
    let check = |item: Evidence| {
        let (sealed, wraps) = report(vec![item])
            .seal(
                &SERVER,
                &REPORT,
                false,
                &reporter,
                &[&moderator.certificate],
            )
            .unwrap();
        open(
            &SERVER,
            &REPORT,
            &reporter_id,
            false,
            &moderator,
            &wraps[0].1,
            &sealed,
        )
        .err()
    };
    assert_eq!(check(genuine()), None);
    let signature = Some(ReportError::Message(MessageError::Signature));
    let authentication = Some(ReportError::Message(MessageError::Authentication));

    // Words put in the sender's mouth: sealed under the disclosed key, but the reporter
    // can only sign with their own device.
    let fake = MessageContext {
        sender_device: reporter.certificate.signing_key().to_bytes(),
        person: *reporter.certificate.person_id(),
        ..context
    };
    let fake_envelope = fake
        .seal(&channel_key, &reporter, &text("I owe you"))
        .unwrap();
    let mut forged = genuine();
    forged.envelope = fake_envelope.clone();
    assert_eq!(check(forged), signature);
    // With the reporter's own certificate too, but still naming the sender.
    let mut own = genuine();
    own.envelope = fake_envelope;
    own.sender = reporter.certificate.clone();
    assert_eq!(
        check(own),
        signature,
        "the person ID still names the sender"
    );

    // A changed ciphertext byte, under the sender's real signature.
    let mut tampered = genuine();
    let at = tampered.envelope.len() - 70;
    tampered.envelope[at] ^= 1;
    assert_eq!(check(tampered), signature);

    // New ciphertext under the disclosed key, re-signed by someone else's device.
    let mut resigned = genuine();
    let (sealed, _) = resigned.envelope.split_last_chunk::<64>().unwrap();
    let mut changed = sealed.to_vec();
    changed[30] ^= 1;
    let aad_and_body = [
        b"rusttalk-message-v1".as_slice(),
        &SERVER,
        &context.channel_id,
        &context.epoch.to_be_bytes(),
        &context.id,
        &context.sender_device,
        &context.person,
        &changed,
    ]
    .concat();
    let signature_bytes = reporter.signing_key.sign(&aad_and_body).to_bytes();
    resigned.envelope = [changed.as_slice(), &signature_bytes].concat();
    assert_eq!(check(resigned), signature);

    // A made-up key for the genuine envelope.
    let mut wrong_key = genuine();
    wrong_key.message_key = Key::new([3; 32]);
    assert_eq!(check(wrong_key), authentication);

    // The genuine envelope relabelled as another message, channel, epoch or sender.
    for relabelled in [
        MessageContext {
            id: [9; 32],
            ..context
        },
        MessageContext {
            channel_id: [9; 32],
            ..context
        },
        MessageContext {
            epoch: 2,
            ..context
        },
    ] {
        let mut item = genuine();
        item.context = relabelled;
        assert_eq!(check(item), signature);
    }
    let other = person("other");
    let mut blamed = genuine();
    blamed.context.person = *other.certificate.person_id();
    blamed.sender = other.certificate.clone();
    assert_eq!(check(blamed), signature);

    // Disclosing refuses an envelope that isn't the sender's.
    let mut bad = envelope.clone();
    bad[40] ^= 1;
    assert_eq!(
        context
            .disclose(&channel_key, &sender.certificate, &bad)
            .err(),
        Some(MessageError::Signature)
    );
    assert_eq!(
        context
            .disclose(&Key::new([2; 32]), &sender.certificate, &envelope)
            .err(),
        Some(MessageError::Authentication)
    );
}

#[test]
fn the_sealed_report_is_bound_to_its_id_reporter_and_server() {
    let (sender, reporter, moderator) = (person("sender"), person("reporter"), person("mod"));
    let channel_key = Key::new([1; 32]);
    let context = context(&sender, 4);
    let envelope = context.seal(&channel_key, &sender, &text("hello")).unwrap();
    let item = evidence(context, &channel_key, &sender, envelope);
    let (sealed, wraps) = report(vec![item])
        .seal(
            &SERVER,
            &REPORT,
            false,
            &reporter,
            &[&moderator.certificate],
        )
        .unwrap();
    let wrap = &wraps[0].1;
    let reporter_id = *reporter.certificate.person_id();
    let other = *person("other").certificate.person_id();
    // Relabelled by the server: another reporter, report or server.
    assert_eq!(
        open(&SERVER, &REPORT, &other, false, &moderator, wrap, &sealed).err(),
        Some(ReportError::Authentication)
    );
    assert_eq!(
        open(
            &SERVER,
            &[9; 32],
            &reporter_id,
            false,
            &moderator,
            wrap,
            &sealed
        )
        .err(),
        Some(ReportError::Authentication)
    );
    assert_eq!(
        open(
            &[9; 32],
            &REPORT,
            &reporter_id,
            false,
            &moderator,
            wrap,
            &sealed
        )
        .err(),
        Some(ReportError::Authentication)
    );
    // A changed byte in the key commitment or the ciphertext, or a changed signature.
    for (at, error) in [
        (30, ReportError::Commitment),
        (80, ReportError::Authentication),
    ] {
        let mut changed = sealed.clone();
        changed[at] ^= 1;
        let opened = open(
            &SERVER,
            &REPORT,
            &reporter_id,
            false,
            &moderator,
            wrap,
            &changed,
        );
        assert_eq!(opened.err(), Some(error), "byte {at}");
    }
    let mut resigned = sealed.clone();
    let last = resigned.len() - 1;
    resigned[last] ^= 1;
    assert_eq!(
        open(
            &SERVER,
            &REPORT,
            &reporter_id,
            false,
            &moderator,
            wrap,
            &resigned
        )
        .err(),
        Some(ReportError::Reporter)
    );
    // A report sealed by someone else's device can't pass as the reporter's.
    let impostor = person("impostor");
    let item = evidence(
        context,
        &channel_key,
        &sender,
        context.seal(&channel_key, &sender, &text("x")).unwrap(),
    );
    let (sealed, wraps) = report(vec![item])
        .seal(
            &SERVER,
            &REPORT,
            false,
            &impostor,
            &[&moderator.certificate],
        )
        .unwrap();
    assert_eq!(
        open(
            &SERVER,
            &REPORT,
            &reporter_id,
            false,
            &moderator,
            &wraps[0].1,
            &sealed
        )
        .err(),
        Some(ReportError::Authentication)
    );
}

#[test]
fn a_shown_edit_must_edit_the_reported_message() {
    let (sender, reporter, moderator) = (person("sender"), person("reporter"), person("mod"));
    let channel_key = Key::new([1; 32]);
    let original = context(&sender, 4);
    let original_envelope = original.seal(&channel_key, &sender, &text("hi")).unwrap();
    let edit = context(&sender, 5);
    let edit_body = |of| MessageBody {
        edit_of: Some(of),
        ..text("changed")
    };
    let reporter_id = *reporter.certificate.person_id();
    let check = |of| {
        let items = vec![
            evidence(original, &channel_key, &sender, original_envelope.clone()),
            evidence(
                edit,
                &channel_key,
                &sender,
                edit.seal(&channel_key, &sender, &edit_body(of)).unwrap(),
            ),
        ];
        let (sealed, wraps) = report(items)
            .seal(
                &SERVER,
                &REPORT,
                false,
                &reporter,
                &[&moderator.certificate],
            )
            .unwrap();
        open(
            &SERVER,
            &REPORT,
            &reporter_id,
            false,
            &moderator,
            &wraps[0].1,
            &sealed,
        )
    };
    let opened = check([4; 32]).unwrap();
    assert_eq!(opened.messages.len(), 2);
    assert_eq!(opened.messages[1].2.text, "changed");
    assert_eq!(check([9; 32]).err(), Some(ReportError::Edit));
}

#[test]
fn limits_hold_the_largest_report() {
    let (sender, reporter, moderator) = (person("sender"), person("reporter"), person("mod"));
    let channel_key = Key::new([1; 32]);
    let context = context(&sender, 4);
    let envelope = context
        .seal(&channel_key, &sender, &text(&"🦀".repeat(2000)))
        .unwrap();
    let mut big = report(vec![
        evidence(context, &channel_key, &sender, envelope.clone()),
        evidence(context, &channel_key, &sender, envelope),
    ]);
    big.note = "🦀".repeat(MAX_NOTE_CHARS);
    let (sealed, _) = big
        .seal(
            &SERVER,
            &REPORT,
            false,
            &reporter,
            &[&moderator.certificate],
        )
        .unwrap();
    assert!(sealed.len() <= MAX_REPORT_BYTES);
    big.note.push('x');
    assert!(matches!(
        big.seal(
            &SERVER,
            &REPORT,
            false,
            &reporter,
            &[&moderator.certificate]
        ),
        Err(ReportError::Encoding)
    ));
    big.note.pop();
    assert!(matches!(
        big.seal(&SERVER, &REPORT, false, &reporter, &[]),
        Err(ReportError::NoReviewers)
    ));
    let json = serde_json::to_string(&NewReport {
        id: "A".repeat(43),
        channel_id: "A".repeat(43),
        message_id: "A".repeat(43),
        edit_id: Some("A".repeat(43)),
        file_ids: vec!["A".repeat(43); MAX_REPORT_FILES],
        body: "A".repeat(MAX_REPORT_BYTES.div_ceil(3) * 4),
        keys: (0..MAX_REVIEWER_DEVICES)
            .map(|_| ReportKey {
                device: "A".repeat(43),
                key: "A".repeat(WRAP_BYTES.div_ceil(3) * 4),
            })
            .collect(),
        omitted: (0..MAX_REVIEWER_DEVICES)
            .map(|_| SignedRevocation {
                person: "A".repeat(43),
                record: "A".repeat(REVOCATION_BYTES.div_ceil(3) * 4),
            })
            .collect(),
    })
    .unwrap();
    assert!(json.len() <= BODY_LIMIT);
}

/// One message from `sender`, reported by `reporter` to `moderator`, as sealed bytes and
/// the moderator's wrap.
fn sealed_report(
    sender: &DeviceIdentity,
    reporter: &DeviceIdentity,
    moderator: &DeviceIdentity,
    dm: bool,
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let channel_key = Key::new([1; 32]);
    let context = context(sender, 4);
    let envelope = context.seal(&channel_key, sender, &text("hello")).unwrap();
    let item = evidence(context, &channel_key, sender, envelope.clone());
    let (sealed, mut wraps) = report(vec![item])
        .seal(&SERVER, &REPORT, dm, reporter, &[&moderator.certificate])
        .unwrap();
    (sealed, wraps.remove(0).1, envelope)
}

#[test]
fn a_dm_report_cant_pass_as_a_channel_report() {
    let (sender, reporter, moderator) = (person("sender"), person("reporter"), person("mod"));
    let reporter_id = *reporter.certificate.person_id();
    let (sealed, wrap, _) = sealed_report(&sender, &reporter, &moderator, true);
    assert!(
        open(
            &SERVER,
            &REPORT,
            &reporter_id,
            true,
            &moderator,
            &wrap,
            &sealed
        )
        .is_ok()
    );
    assert_eq!(
        open(
            &SERVER,
            &REPORT,
            &reporter_id,
            false,
            &moderator,
            &wrap,
            &sealed
        )
        .err(),
        Some(ReportError::Authentication)
    );
}

#[test]
fn every_reviewer_must_unwrap_the_committed_key() {
    let (sender, reporter, moderator) = (person("sender"), person("reporter"), person("mod"));
    let reporter_id = *reporter.certificate.person_id();
    let (sealed, _, _) = sealed_report(&sender, &reporter, &moderator, false);
    // A wrap of some other key, as a reporter giving one reviewer a different report would
    // need.
    let other = super::sealed::wrap(&SERVER, &REPORT, &moderator.certificate, &Key::new([5; 32]));
    assert_eq!(
        open(
            &SERVER,
            &REPORT,
            &reporter_id,
            false,
            &moderator,
            &other.unwrap(),
            &sealed
        )
        .err(),
        Some(ReportError::Commitment)
    );
}

#[test]
fn the_primary_message_is_never_an_edit_and_an_edit_is_another_record() {
    let (sender, reporter, moderator) = (person("sender"), person("reporter"), person("mod"));
    let channel_key = Key::new([1; 32]);
    let reporter_id = *reporter.certificate.person_id();
    let edit_body = |of| MessageBody {
        edit_of: Some(of),
        ..text("changed")
    };
    let check = |items: Vec<Evidence>| {
        let (sealed, wraps) = report(items)
            .seal(
                &SERVER,
                &REPORT,
                false,
                &reporter,
                &[&moderator.certificate],
            )
            .unwrap();
        open(
            &SERVER,
            &REPORT,
            &reporter_id,
            false,
            &moderator,
            &wraps[0].1,
            &sealed,
        )
        .err()
    };
    // An edit reported as the message itself.
    let edit = context(&sender, 5);
    let envelope = edit
        .seal(&channel_key, &sender, &edit_body([4; 32]))
        .unwrap();
    assert_eq!(
        check(vec![evidence(
            edit,
            &channel_key,
            &sender,
            envelope.clone()
        )]),
        Some(ReportError::Edit)
    );
    // The same record given as both the message and its edit.
    let original = context(&sender, 4);
    let first = original.seal(&channel_key, &sender, &text("hi")).unwrap();
    let selfish = original
        .seal(&channel_key, &sender, &edit_body([4; 32]))
        .unwrap();
    assert_eq!(
        check(vec![
            evidence(original, &channel_key, &sender, first),
            evidence(original, &channel_key, &sender, selfish),
        ]),
        Some(ReportError::Edit)
    );
}

#[test]
fn evidence_must_match_the_message_the_server_stored() {
    use sha2::{Digest, Sha256};
    let (sender, reporter, moderator) = (person("sender"), person("reporter"), person("mod"));
    let reporter_id = *reporter.certificate.person_id();
    let (sealed, wrap, envelope) = sealed_report(&sender, &reporter, &moderator, false);
    let opened = open(
        &SERVER,
        &REPORT,
        &reporter_id,
        false,
        &moderator,
        &wrap,
        &sealed,
    )
    .unwrap();
    let hash: [u8; 32] = Sha256::digest(&envelope).into();
    let stored = StoredMessage {
        id: encode(&[4; 32]),
        device: encode(sender.certificate.signing_key().as_bytes()),
        epoch: 1,
        hash: encode(&hash),
    };
    let record = |stored: Vec<StoredMessage>| ReportRecord {
        id: encode(&REPORT),
        channel_id: encode(&[3; 32]),
        message_id: encode(&[4; 32]),
        edit_id: None,
        reporter: encode(&reporter_id),
        sender: encode(sender.certificate.person_id()),
        dm: false,
        at: 0,
        body: String::new(),
        key: None,
        stored,
        revocations: Vec::new(),
    };
    assert_eq!(opened.check_listing(&record(vec![stored.clone()])), Ok(()));
    // A different envelope under the same ID (say, signed later by a removed device), a
    // different device or epoch, or no stored message at all.
    for changed in [
        StoredMessage {
            hash: encode(&[0; 32]),
            ..stored.clone()
        },
        StoredMessage {
            device: encode(&[0; 32]),
            ..stored.clone()
        },
        StoredMessage {
            epoch: 2,
            ..stored.clone()
        },
    ] {
        assert_eq!(
            opened.check_listing(&record(vec![changed])),
            Err(ReportError::Stored)
        );
    }
    assert_eq!(
        opened.check_listing(&record(Vec::new())),
        Err(ReportError::Stored)
    );
    let mut relabelled = record(vec![stored]);
    relabelled.message_id = encode(&[9; 32]);
    assert_eq!(opened.check_listing(&relabelled), Err(ReportError::Stored));
}
