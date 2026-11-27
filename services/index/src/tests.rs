use ed25519_dalek::SigningKey;
use rusttalk_core::auth::encode;

use super::*;

const NOW: i64 = 1_800_000_000_000;

fn listing(key: &SigningKey) -> Listing {
    Listing {
        server_id: encode(key.verifying_key().as_bytes()),
        name: "Test % server".into(),
        description: "Welcome".into(),
        address: "https://[2001:db8::1]:8443".into(),
        member_count: 3,
        open_joining: false,
    }
}

#[test]
fn signatures_bind_the_server_and_public_fields() {
    let key = SigningKey::from_bytes(&[1; 32]);
    let other = SigningKey::from_bytes(&[2; 32]);
    let wrong_key = Registration::signed(listing(&key), NOW, &other).unwrap();
    assert_eq!(wrong_key.verify(NOW), Err(RegistrationError::Signature));

    let mut record = Registration::signed(listing(&key), NOW, &key).unwrap();
    assert_eq!(record.verify(NOW), Ok(()));
    for offset in [-CLOCK_SKEW_MS - 1, CLOCK_SKEW_MS + 1] {
        assert_eq!(
            record.verify(NOW + offset),
            Err(RegistrationError::Timestamp)
        );
    }
    record.listing.member_count += 1;
    assert_eq!(record.verify(NOW), Err(RegistrationError::Signature));
    record.listing.member_count -= 1;
    record.timestamp += 1;
    assert_eq!(record.verify(NOW), Err(RegistrationError::Signature));
}

#[test]
fn rejects_oversized_fields_and_non_origin_addresses() {
    let key = SigningKey::from_bytes(&[1; 32]);
    let signed = || Registration::signed(listing(&key), NOW, &key).unwrap();
    let mut record = signed();
    record.listing.name = "é".repeat(101);
    assert_eq!(record.verify(NOW), Err(RegistrationError::Fields));
    let mut record = signed();
    record.listing.description = "😀".repeat(1_001);
    assert_eq!(record.verify(NOW), Err(RegistrationError::Fields));
    for text in [
        "evil\u{202E}gnp.exe",
        "Clo\u{200B}ver",
        "\u{2066}Clover\u{2069}",
    ] {
        let mut record = signed();
        record.listing.name = text.into();
        assert_eq!(record.verify(NOW), Err(RegistrationError::Fields));
        let mut record = signed();
        record.listing.description = text.into();
        assert_eq!(record.verify(NOW), Err(RegistrationError::Fields));
    }
    let mut record = signed();
    record.listing.description = "שלום\u{200F} line\r\n\tnext".into();
    record = Registration::signed(record.listing, NOW, &key).unwrap();
    assert_eq!(record.verify(NOW), Ok(()));
    for address in [
        "http://example.org",
        "https://me@example.org",
        "https://example.org/a/..",
        "https://example.org#",
        "https://example.org:0",
        "https:////example.org",
        &format!("https://{}", "a".repeat(505)),
    ] {
        let mut record = signed();
        record.listing.address = address.into();
        assert_eq!(record.verify(NOW), Err(RegistrationError::Fields));
    }
}

#[tokio::test]
async fn search_hides_expired_entries_and_refresh_rejects_replays() {
    let db = open("sqlite::memory:".parse().unwrap()).await.unwrap();
    let key = SigningKey::from_bytes(&[1; 32]);
    let signed = |timestamp| Registration::signed(listing(&key), timestamp, &key).unwrap();
    let ttl = 8_000;
    save(&db, signed(NOW), NOW, ttl, MAX_LISTINGS)
        .await
        .unwrap();
    save(&db, signed(NOW + 1), NOW + 1, ttl, MAX_LISTINGS)
        .await
        .unwrap();
    assert_eq!(list(&db, "%", 1, NOW + ttl).await.unwrap().len(), 1);
    assert_eq!(list(&db, "welcome", 1, NOW + ttl).await.unwrap().len(), 1);
    assert!(list(&db, "_", 1, NOW).await.unwrap().is_empty());
    assert!(list(&db, "%", 2, NOW).await.unwrap().is_empty());
    let expired = NOW + ttl + 1;
    assert!(list(&db, "", 1, expired).await.unwrap().is_empty());
    prune_expired(&db, expired).await.unwrap();
    for timestamp in [NOW, NOW + 1] {
        assert_eq!(
            save(&db, signed(timestamp), expired, ttl, MAX_LISTINGS).await,
            Err(IndexError(StatusCode::CONFLICT, "replayed_registration").into())
        );
    }
    let stale = NOW + CLOCK_SKEW_MS + 2;
    prune_expired(&db, stale).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM listings")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        save(&db, signed(NOW + 1), stale, ttl, MAX_LISTINGS).await,
        Err(IndexError(StatusCode::BAD_REQUEST, "stale_registration").into())
    );
}

#[tokio::test]
async fn new_servers_are_refused_at_the_listing_cap_but_refreshes_continue() {
    let db = open("sqlite::memory:".parse().unwrap()).await.unwrap();
    let first = SigningKey::from_bytes(&[1; 32]);
    let second = SigningKey::from_bytes(&[2; 32]);
    let signed =
        |key: &SigningKey, timestamp| Registration::signed(listing(key), timestamp, key).unwrap();
    save(&db, signed(&first, NOW), NOW, 8_000, 1).await.unwrap();
    assert_eq!(
        save(&db, signed(&second, NOW), NOW, 8_000, 1).await,
        Err(IndexError(StatusCode::SERVICE_UNAVAILABLE, "index_full").into())
    );
    save(&db, signed(&first, NOW + 1), NOW + 1, 8_000, 1)
        .await
        .unwrap();
    assert_eq!(
        save(&db, signed(&first, NOW + 1), NOW + 1, 8_000, 1).await,
        Err(IndexError(StatusCode::CONFLICT, "replayed_registration").into())
    );
    let later = NOW + CLOCK_SKEW_MS + 8_002;
    prune_expired(&db, later).await.unwrap();
    save(&db, signed(&second, later), later, 8_000, 1)
        .await
        .unwrap();
}

#[test]
fn rate_limit_groups_ipv6_prefixes_and_resets() {
    let mut limit = RateLimit::new(Duration::from_secs(60), 16);
    for _ in 0..16 {
        assert!(limit.allow("2001:db8:1:2::1".parse().unwrap()));
    }
    assert!(!limit.allow("2001:db8:1:2::2".parse().unwrap()));
    assert!(limit.allow("2001:db8:1:3::1".parse().unwrap()));
    limit.started -= Duration::from_secs(60);
    assert!(limit.allow("2001:db8:1:2::2".parse().unwrap()));
}

fn report(reason: &str, note: &str) -> moderation::Report {
    moderation::Report {
        reason: reason.into(),
        note: note.into(),
    }
}

fn sanction(action: &str, reason: &str) -> moderation::Sanction {
    moderation::Sanction {
        action: action.into(),
        reason: reason.into(),
        address: None,
    }
}

#[tokio::test]
async fn reports_need_a_listed_server_a_known_reason_and_plain_text() {
    let db = open("sqlite::memory:".parse().unwrap()).await.unwrap();
    let key = SigningKey::from_bytes(&[1; 32]);
    let id = encode(key.verifying_key().as_bytes());
    let unknown = encode(SigningKey::from_bytes(&[2; 32]).verifying_key().as_bytes());
    let signed = Registration::signed(listing(&key), NOW, &key).unwrap();
    save(&db, signed, NOW, 8_000, MAX_LISTINGS).await.unwrap();

    let refused = |status, code| Err(IndexError(status, code));
    assert_eq!(
        moderation::save_report(&db, &unknown, report("spam", ""), NOW).await,
        refused(StatusCode::NOT_FOUND, "unknown_server")
    );
    assert_eq!(
        moderation::save_report(&db, "not-an-id", report("spam", ""), NOW).await,
        refused(StatusCode::BAD_REQUEST, "invalid_server_id")
    );
    let long = "a".repeat(501);
    for (reason, note) in [
        ("rude", ""),
        ("spam", "Clo\u{200B}ver"),
        ("spam", long.as_str()),
    ] {
        assert_eq!(
            moderation::save_report(&db, &id, report(reason, note), NOW).await,
            refused(StatusCode::BAD_REQUEST, "invalid_report")
        );
    }
    moderation::save_report(&db, &id, report("scam", " Asks for\npasswords "), NOW)
        .await
        .unwrap();
    let overview = moderation::overview(&db).await.unwrap();
    assert_eq!(overview.reports.len(), 1);
    assert_eq!(overview.reports[0].server_id, id);
    assert_eq!(overview.reports[0].reason, "scam");
    assert_eq!(overview.reports[0].note, "Asks for\npasswords");
}

#[test]
fn report_limit_allows_five_per_window() {
    let mut limit = RateLimit::new(Duration::from_secs(600), 5);
    let ip = "192.0.2.1".parse().unwrap();
    for _ in 0..5 {
        assert!(limit.allow(ip));
    }
    assert!(!limit.allow(ip));
    limit.started -= Duration::from_secs(599);
    assert!(!limit.allow(ip));
    limit.started -= Duration::from_secs(1);
    assert!(limit.allow(ip));
}

#[tokio::test]
async fn delisted_servers_are_refused_with_the_reason_until_restored() {
    let db = open("sqlite::memory:".parse().unwrap()).await.unwrap();
    let key = SigningKey::from_bytes(&[1; 32]);
    let id = encode(key.verifying_key().as_bytes());
    let signed = |key: &SigningKey, timestamp| {
        let mut listing = listing(key);
        listing.name = format!("Renamed {timestamp}");
        Registration::signed(listing, timestamp, key).unwrap()
    };
    save(&db, signed(&key, NOW), NOW, 8_000, MAX_LISTINGS)
        .await
        .unwrap();
    moderation::save_report(&db, &id, report("spam", ""), NOW)
        .await
        .unwrap();
    assert_eq!(
        moderation::place(&db, &id, sanction("hide", "Spam"), NOW).await,
        Err(IndexError(StatusCode::BAD_REQUEST, "invalid_sanction"))
    );
    assert_eq!(
        moderation::place(&db, &id, sanction("delist", "  "), NOW).await,
        Err(IndexError(StatusCode::BAD_REQUEST, "invalid_sanction"))
    );
    moderation::place(&db, &id, sanction("delist", " Spam links "), NOW)
        .await
        .unwrap();
    assert!(list(&db, "", 1, NOW).await.unwrap().is_empty());
    let overview = moderation::overview(&db).await.unwrap();
    assert!(overview.reports.is_empty(), "settled with the delist");
    assert_eq!(overview.sanctions[0].reason, "Spam links");
    assert_eq!(overview.sanctions[0].host, None);
    // A new name does not get it back in.
    assert_eq!(
        save(&db, signed(&key, NOW + 1), NOW + 1, 8_000, MAX_LISTINGS).await,
        Err(Refused::Delisted("Spam links".into()))
    );
    // A delist is by server ID only: another server at the same address may list.
    let other = SigningKey::from_bytes(&[2; 32]);
    save(&db, signed(&other, NOW + 1), NOW + 1, 8_000, MAX_LISTINGS)
        .await
        .unwrap();

    moderation::lift(&db, &id).await.unwrap();
    assert_eq!(
        moderation::lift(&db, &id).await,
        Err(IndexError(StatusCode::NOT_FOUND, "not_removed"))
    );
    save(&db, signed(&key, NOW + 2), NOW + 2, 8_000, MAX_LISTINGS)
        .await
        .unwrap();
    assert_eq!(list(&db, "", 1, NOW + 2).await.unwrap().len(), 2);
}

#[tokio::test]
async fn blocks_also_refuse_new_keys_at_the_same_host() {
    let db = open("sqlite::memory:".parse().unwrap()).await.unwrap();
    let key = SigningKey::from_bytes(&[1; 32]);
    let id = encode(key.verifying_key().as_bytes());
    let elsewhere = |key: &SigningKey, address: &str, timestamp| {
        let mut listing = listing(key);
        listing.address = address.into();
        Registration::signed(listing, timestamp, key).unwrap()
    };
    let neighbour = SigningKey::from_bytes(&[3; 32]);
    let stranger = SigningKey::from_bytes(&[4; 32]);
    for (key, address) in [
        (&key, "https://Bad.example.org"),
        (&neighbour, "https://bad.example.org:8443"),
        (&stranger, "https://good.example.org"),
    ] {
        save(&db, elsewhere(key, address, NOW), NOW, 8_000, MAX_LISTINGS)
            .await
            .unwrap();
    }
    // A server never seen needs its address.
    let unseen = encode(SigningKey::from_bytes(&[5; 32]).verifying_key().as_bytes());
    assert_eq!(
        moderation::place(&db, &unseen, sanction("block", "Scam"), NOW).await,
        Err(IndexError(StatusCode::BAD_REQUEST, "address_required"))
    );
    moderation::place(&db, &id, sanction("block", "Scam"), NOW)
        .await
        .unwrap();
    let left = list(&db, "", 1, NOW).await.unwrap();
    assert_eq!(left.len(), 1, "the whole host is gone");
    assert_eq!(left[0].address, "https://good.example.org");
    assert_eq!(
        moderation::overview(&db).await.unwrap().sanctions[0]
            .host
            .as_deref(),
        Some("bad.example.org")
    );
    let fresh = SigningKey::from_bytes(&[6; 32]);
    assert_eq!(
        save(
            &db,
            elsewhere(&fresh, "https://BAD.example.org:9443", NOW + 1),
            NOW + 1,
            8_000,
            MAX_LISTINGS
        )
        .await,
        Err(Refused::Delisted("Scam".into()))
    );
    moderation::lift(&db, &id).await.unwrap();
    save(
        &db,
        elsewhere(&fresh, "https://bad.example.org", NOW + 2),
        NOW + 2,
        8_000,
        MAX_LISTINGS,
    )
    .await
    .unwrap();
}
