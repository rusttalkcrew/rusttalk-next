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
    save(&db, signed(NOW), NOW, ttl).await.unwrap();
    save(&db, signed(NOW + 1), NOW + 1, ttl).await.unwrap();
    assert_eq!(list(&db, "%", 1, NOW + ttl).await.unwrap().len(), 1);
    assert_eq!(list(&db, "welcome", 1, NOW + ttl).await.unwrap().len(), 1);
    assert!(list(&db, "_", 1, NOW).await.unwrap().is_empty());
    assert!(list(&db, "%", 2, NOW).await.unwrap().is_empty());
    let expired = NOW + ttl + 1;
    assert!(list(&db, "", 1, expired).await.unwrap().is_empty());
    prune_expired(&db, expired).await.unwrap();
    for timestamp in [NOW, NOW + 1] {
        assert_eq!(
            save(&db, signed(timestamp), expired, ttl).await,
            Err(IndexError(StatusCode::CONFLICT, "replayed_registration"))
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
        save(&db, signed(NOW + 1), stale, ttl).await,
        Err(IndexError(StatusCode::BAD_REQUEST, "stale_registration"))
    );
}

#[test]
fn rate_limit_groups_ipv6_prefixes_and_resets() {
    let mut limit = RateLimit {
        started: Instant::now(),
        counts: HashMap::new(),
    };
    for _ in 0..16 {
        assert!(limit.allow("2001:db8:1:2::1".parse().unwrap()));
    }
    assert!(!limit.allow("2001:db8:1:2::2".parse().unwrap()));
    assert!(limit.allow("2001:db8:1:3::1".parse().unwrap()));
    limit.started -= Duration::from_secs(60);
    assert!(limit.allow("2001:db8:1:2::2".parse().unwrap()));
}
