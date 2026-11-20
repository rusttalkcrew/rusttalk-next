use std::{env, path::Path, process::Command};

use anyhow::{Context, Result, ensure};
use reqwest::{Method, Response, StatusCode};
use rustls_pki_types::{CertificateDer, pem::PemObject};
use rusttalk_bot::membership::Bot;
use rusttalk_core::{
    auth::{InviteCode, InviteLink, KnownServer, ProtocolError, Session, decode, encode},
    identity::{DeviceIdentity, revoke_device},
    pinned_tls::fingerprint,
};
use serde_json::{Value, json};

fn compose(project: &str, args: &[&str]) -> Result<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new("docker")
        .current_dir(root)
        .args(["compose", "-f", "deploy/compose.dev.yml", "-p", project])
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "compose {} failed: {}",
        args[0],
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

async fn rejected(reply: Response, status: StatusCode, code: &str) -> Result<()> {
    assert_eq!(reply.status(), status);
    let body: Value = reply.json().await?;
    assert!(body["error"] == code, "expected rejection {code}");
    Ok(())
}

async fn session(reply: Response, secrets: &mut Vec<String>) -> Result<Session> {
    assert_eq!(reply.status(), StatusCode::OK);
    let session: Session = reply.json().await?;
    secrets.push(session.token.clone());
    Ok(session)
}

async fn invite(
    bot: &Bot,
    owner: &Session,
    secrets: &mut Vec<String>,
) -> Result<(String, InviteCode)> {
    let reply = bot
        .request(Method::POST, "/v1/invites")
        .bearer_auth(&owner.token)
        .json(&json!({"uses": 1, "ttl_seconds": 300}))
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::CREATED);
    let invite: Value = reply.json().await?;
    let code = invite["code"].as_str().context("invite code missing")?;
    secrets.push(code.to_owned());
    Ok((
        invite["id"]
            .as_str()
            .context("invite ID missing")?
            .to_owned(),
        decode(code)?,
    ))
}

async fn check_me(bot: &Bot, session: &Session, status: StatusCode) -> Result<()> {
    let reply = bot
        .request(Method::GET, "/v1/me")
        .bearer_auth(&session.token)
        .send()
        .await?;
    assert_eq!(reply.status(), status);
    Ok(())
}

#[tokio::test]
async fn docker_membership() -> Result<()> {
    let Ok(project) = env::var("RUSTTALK_E2E_PROJECT") else {
        return Ok(());
    };
    let data = env::var("RUSTTALK_DEV_DATA")?;
    let data = Path::new(&data);
    let cert = CertificateDer::from_pem_file(data.join("tls/cert.pem"))?;
    let address = "https://127.0.0.1:8443/".parse()?;
    let cli = |command| compose(&project, &["exec", "-T", "api", "rusttalk-api", command]);
    let known = KnownServer {
        address,
        server_id: decode(&cli("server-id")?)?,
        tls_fingerprint: Some(fingerprint(cert.as_ref())),
    };
    let owner_pins = data.join("bot-a");
    let member_pins = data.join("bot-b");
    let owner_bot = Bot::connect(
        known.address.clone(),
        known.tls_fingerprint,
        Some(&known),
        &owner_pins,
    )
    .await?;
    let (_, owner_id) = DeviceIdentity::create("Bot A")?;
    let old_code = cli("owner-invite")?;
    let owner_code = cli("owner-invite")?;
    let mut secrets = vec![old_code.clone(), owner_code.clone()];
    rejected(
        owner_bot
            .sign_in(&owner_id, Some(&decode(&old_code)?))
            .await?,
        StatusCode::FORBIDDEN,
        "invalid_invite",
    )
    .await?;
    let owner = session(
        owner_bot
            .sign_in(&owner_id, Some(&decode(&owner_code)?))
            .await?,
        &mut secrets,
    )
    .await?;
    assert!(cli("owner-invite")?.is_empty());
    println!("owner claimed via CLI invite; the previous invite and further issuance were refused");

    let (_, first_invite) = invite(&owner_bot, &owner, &mut secrets).await?;
    let link = InviteLink::parse(
        &InviteLink {
            server: known.clone(),
            code: first_invite,
        }
        .to_string(),
    )?;
    let member_bot = Bot::connect(
        link.server.address.clone(),
        link.server.tls_fingerprint,
        Some(&link.server),
        &member_pins,
    )
    .await?;
    let (member_words, member_id) = DeviceIdentity::create("Bot B")?;
    let initial = session(
        member_bot.sign_in(&member_id, Some(&link.code)).await?,
        &mut secrets,
    )
    .await?;
    check_me(&member_bot, &initial, StatusCode::OK).await?;
    let initial = {
        let reply = member_bot
            .request(Method::POST, "/v1/auth/refresh")
            .header("x-forwarded-for", "192.0.2.1, 192.0.2.2")
            .bearer_auth(&initial.token)
            .send()
            .await?;
        let refreshed = session(reply, &mut secrets).await?;
        check_me(&member_bot, &initial, StatusCode::UNAUTHORIZED).await?;
        refreshed
    };
    println!(
        "owner issued a member invite; bot B joined and rotated its token through Caddy with a spoofed XFF header"
    );

    let member_person = encode(member_id.certificate.person_id());
    let member_path = format!("/v1/members/{member_person}");
    let ban_path = format!("/v1/bans/{member_person}");
    let reply = owner_bot
        .request(Method::DELETE, &member_path)
        .bearer_auth(&owner.token)
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::NO_CONTENT);
    check_me(&member_bot, &initial, StatusCode::UNAUTHORIZED).await?;
    rejected(
        member_bot.sign_in(&member_id, None).await?,
        StatusCode::FORBIDDEN,
        "invite_required",
    )
    .await?;
    rejected(
        member_bot.sign_in(&member_id, Some(&first_invite)).await?,
        StatusCode::FORBIDDEN,
        "invalid_invite",
    )
    .await?;
    println!("kick dropped bot B's token and required an unused invite");

    let (_, second_invite) = invite(&owner_bot, &owner, &mut secrets).await?;
    let rejoined = session(
        member_bot.sign_in(&member_id, Some(&second_invite)).await?,
        &mut secrets,
    )
    .await?;
    println!("bot B rejoined with a fresh invite");

    let reply = owner_bot
        .request(Method::PUT, &ban_path)
        .bearer_auth(&owner.token)
        .json(&json!({"ban_ip": true}))
        .send()
        .await?;
    rejected(reply, StatusCode::FORBIDDEN, "role_hierarchy").await?;
    let reply = owner_bot
        .request(Method::PUT, &ban_path)
        .bearer_auth(&owner.token)
        .json(&json!({"ban_ip": false}))
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::NO_CONTENT);
    check_me(&member_bot, &rejoined, StatusCode::UNAUTHORIZED).await?;
    let (_, third_invite) = invite(&owner_bot, &owner, &mut secrets).await?;
    rejected(
        member_bot.sign_in(&member_id, Some(&third_invite)).await?,
        StatusCode::FORBIDDEN,
        "person_banned",
    )
    .await?;
    println!("ban dropped bot B's token and blocked redemption of a valid invite");

    let reply = owner_bot
        .request(Method::DELETE, &ban_path)
        .bearer_auth(&owner.token)
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::NO_CONTENT);
    let unbanned = session(
        member_bot.sign_in(&member_id, Some(&third_invite)).await?,
        &mut secrets,
    )
    .await?;
    check_me(&member_bot, &unbanned, StatusCode::OK).await?;
    println!("unban let bot B redeem the same invite and sign in again");

    let (invite_id, withdrawn) = invite(&owner_bot, &owner, &mut secrets).await?;
    let reply = owner_bot
        .request(Method::DELETE, &format!("/v1/invites/{invite_id}"))
        .bearer_auth(&owner.token)
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::NO_CONTENT);
    let (_, outsider) = DeviceIdentity::create("Outsider")?;
    rejected(
        member_bot.sign_in(&outsider, Some(&withdrawn)).await?,
        StatusCode::FORBIDDEN,
        "invalid_invite",
    )
    .await?;
    let sibling = DeviceIdentity::add_device(&member_words, "Sibling")?;
    let sibling_token = session(member_bot.sign_in(&sibling, None).await?, &mut secrets).await?;
    let revocation = revoke_device(
        &member_words,
        member_id.certificate.signing_key().to_bytes(),
    )?;
    let reply = member_bot
        .request(Method::POST, "/v1/me/revocations")
        .bearer_auth(&sibling_token.token)
        .json(&json!({"record": encode(&revocation.to_bytes())}))
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::NO_CONTENT);
    check_me(&member_bot, &unbanned, StatusCode::UNAUTHORIZED).await?;
    check_me(&member_bot, &sibling_token, StatusCode::OK).await?;
    rejected(
        member_bot.sign_in(&member_id, None).await?,
        StatusCode::FORBIDDEN,
        "device_revoked",
    )
    .await?;
    println!(
        "revoked invite was refused; a sibling device revoked bot B's original device and token"
    );

    let reply = owner_bot
        .request(Method::POST, "/v1/roles")
        .bearer_auth(&owner.token)
        .json(&json!({"rank": 25, "permissions": ["invite", "manage_roles"]}))
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::CREATED);
    let role: Value = reply.json().await?;
    let role_id = role["id"].as_str().context("role ID missing")?;
    let reply = owner_bot
        .request(Method::PUT, "/v1/roles/default")
        .bearer_auth(&owner.token)
        .json(&json!({"role_id": role_id}))
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::NO_CONTENT);
    let listed: Value = owner_bot
        .request(Method::GET, "/v1/roles")
        .bearer_auth(&owner.token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(listed["default_role_id"] == role_id);
    let reply = owner_bot
        .request(Method::PUT, &format!("{member_path}/roles"))
        .bearer_auth(&owner.token)
        .json(&json!({"role_ids": [role_id, "member"]}))
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::NO_CONTENT);
    let owner_person = encode(owner_id.certificate.person_id());
    let reply = member_bot
        .request(Method::PUT, &format!("/v1/members/{owner_person}/roles"))
        .bearer_auth(&sibling_token.token)
        .json(&json!({"role_ids": ["member"]}))
        .send()
        .await?;
    rejected(reply, StatusCode::FORBIDDEN, "role_hierarchy").await?;
    let transfer = json!({"person_id": member_person});
    let reply = owner_bot
        .request(Method::PUT, "/v1/owner")
        .bearer_auth(&owner.token)
        .json(&transfer)
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::NO_CONTENT);
    let reply = owner_bot
        .request(Method::PUT, "/v1/owner")
        .bearer_auth(&owner.token)
        .json(&transfer)
        .send()
        .await?;
    rejected(reply, StatusCode::FORBIDDEN, "owner_required").await?;

    let reply = member_bot
        .request(Method::GET, "/v1/audit")
        .bearer_auth(&sibling_token.token)
        .send()
        .await?;
    let audit: Vec<Value> = reply.error_for_status()?.json().await?;
    for action in [
        "member_kicked",
        "member_banned",
        "member_unbanned",
        "device_revoked",
        "member_roles_changed",
        "ownership_transferred",
    ] {
        assert!(
            audit.iter().any(|event| event["action"] == action),
            "missing audit action {action}"
        );
    }
    println!(
        "custom role assignment and default worked, ownership transferred, and audit recorded moderation"
    );

    compose(&project, &["stop", "api"])?;
    compose(
        &project,
        &[
            "run",
            "--rm",
            "--no-deps",
            "--entrypoint",
            "rm",
            "api",
            "/data/server.key",
        ],
    )?;
    compose(&project, &["up", "--wait", "--wait-timeout", "60"])?;
    assert!(decode::<32>(&cli("server-id")?)? != known.server_id);
    let result = Bot::connect(
        known.address.clone(),
        known.tls_fingerprint,
        None,
        &member_pins,
    )
    .await;
    assert!(
        matches!(result, Err(error) if error.downcast_ref::<ProtocolError>() == Some(&ProtocolError::ServerIdChanged))
    );
    println!("bot refused the replacement server key using its saved pin");

    let logs = compose(&project, &["logs", "--no-color", "api"])?;
    for secret in secrets {
        assert!(
            !logs.contains(&secret),
            "invite or token appeared in API logs"
        );
    }
    for reason in ["refreshed", "kicked", "banned", "revoked"] {
        assert!(
            logs.contains(&format!("\"reason\":\"{reason}\"")),
            "missing session close reason {reason}"
        );
    }
    println!(
        "API logs contained session-close reasons and none of the issued invite codes or tokens"
    );
    Ok(())
}
