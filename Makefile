.DEFAULT_GOAL := help
.PHONY: help dev client bot logs down reset lint fmt test e2e soak release release-key

DEV_COMPOSE := docker compose -f deploy/compose.dev.yml --profile index -p rusttalk-dev
export RUSTTALK_DEV_USER := $(shell id -u):$(shell id -g)
CARGO_SCOPE ?= --workspace
E2E_FEATURES ?= --features media
ifeq ($(shell uname -s),Linux)
PIPEWIRE_HELPER := -p rusttalk-pipewire
endif
# macOS's own openssl is LibreSSL, which can't make Ed25519 keys; the updater tests need
# Homebrew's OpenSSL 3.
ifeq ($(shell uname -s),Darwin)
OPENSSL_PATH := $(shell brew --prefix openssl@3 2>/dev/null)/bin:
endif

##@ Help

help: ## Show this help
	@awk 'BEGIN {FS = ":.*## "} \
		/^##@/ {printf "\n\033[1;33m%s\033[0m\n", substr($$0, 5)} \
		/^[a-zA-Z0-9_-]+:.*##/ {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo

##@ Dev

ifeq ($(shell uname -s),Darwin)
dev: export NODE_IP ?= $(shell ipconfig getifaddr "$$(route -n get 1.1.1.1 2>/dev/null | awk '/interface:/ { print $$2 }')" 2>/dev/null)
else
dev: export NODE_IP ?= $(shell ip -4 route get 1.1.1.1 2>/dev/null | awk '{ for (i = 1; i < NF; i++) if ($$i == "src") { print $$(i + 1); exit } }')
endif
dev: export RUSTTALK_DEV_HTTPS_PORT ?= 8443
dev: ## Start the Docker stack and rebuild the API on changes
	@test -n "$$NODE_IP" || { printf '\033[1;31mSet NODE_IP to the host LAN IPv4 address.\033[0m\n' >&2; exit 1; }
	@mkdir -p .dev
	@printf '\033[1;34m==> Starting dev stack\033[0m\n'
	$(DEV_COMPOSE) up --build --wait --wait-timeout 60
	@fingerprint=$$($(DEV_COMPOSE) run --rm --no-deps -T --entrypoint openssl cert \
		x509 -in /dev-data/tls/cert.pem -noout -sha256 -fingerprint) && \
		printf '\033[1;34m==> https://127.0.0.1:%s\n==> Index https://127.0.0.1:%s/index/\n==> LAN wss://%s:%s\n==> TLS %s\033[0m\n' "$$RUSTTALK_DEV_HTTPS_PORT" "$$RUSTTALK_DEV_HTTPS_PORT" "$$NODE_IP" "$$RUSTTALK_DEV_HTTPS_PORT" "$$fingerprint"
	@server=$$($(DEV_COMPOSE) exec -T api rusttalk-api server-fingerprint) && \
		printf '\033[1;34m==> Server fingerprint %s\033[0m\n' "$$server"
	@invite=$$($(DEV_COMPOSE) exec -T api rusttalk-api owner-invite) && \
		if [ -n "$$invite" ]; then \
			server_id=$$($(DEV_COMPOSE) exec -T api rusttalk-api server-id) && \
			tls_pin=$$($(DEV_COMPOSE) run --rm --no-deps -T --entrypoint /bin/sh cert -ec \
				'set -o pipefail; openssl x509 -in /dev-data/tls/cert.pem -outform DER | openssl dgst -sha256 -binary | openssl base64 -A | tr "/+" "_-" | tr -d "="') && \
			printf '\033[1;34m==> Owner invite rusttalk://127.0.0.1:%s/i/%s#%s.%s\033[0m\n' "$$RUSTTALK_DEV_HTTPS_PORT" "$$invite" "$$server_id" "$$tls_pin"; \
		fi
	$(DEV_COMPOSE) watch --no-up api

client: ## Run the native client, optionally with PROFILE=a to z
	@case "$${PROFILE+x}:$$PROFILE" in \
		:) unset RUSTTALK_PROFILE ;; \
		x:[abcdefghijklmnopqrstuvwxyz]) export RUSTTALK_PROFILE="$$PROFILE" ;; \
		*) printf '\033[1;31mPROFILE must be one lowercase letter (a-z).\033[0m\n' >&2; exit 1 ;; \
	esac; \
	printf '\033[1;34m==> Starting native client\033[0m\n'; \
	$(if $(PIPEWIRE_HELPER),cargo build --locked $(PIPEWIRE_HELPER) &&) cargo run --locked -p rusttalk-client

bot: export CARGO_TARGET_DIR ?= target
bot: export CHANNEL := $(CHANNEL)
bot: ## Exchange tones as two bots using RUSTTALK_BOT_INVITE and CHANNEL
	@test -n "$$RUSTTALK_BOT_INVITE" && test -n "$$CHANNEL" || { printf 'Set RUSTTALK_BOT_INVITE and CHANNEL.\n' >&2; exit 1; }
	@printf '\033[1;34m==> Starting dev bots\033[0m\n'
	cargo build --locked -p rusttalk-bot $(PIPEWIRE_HELPER)
	@bash -ec '"$$CARGO_TARGET_DIR/debug/rusttalk-bot" --identity bot-a --channel "$$CHANNEL" --tone 440 --expect-tone 660 & first=$$!; \
		"$$CARGO_TARGET_DIR/debug/rusttalk-bot" --identity bot-b --channel "$$CHANNEL" --tone 660 --expect-tone 440 & second=$$!; \
		trap "kill -TERM $$first $$second 2>/dev/null || true; wait || true" EXIT; \
		trap "exit 130" INT; trap "exit 143" TERM; \
		wait -n'

logs: ## Follow all dev service logs
	@printf '\033[1;34m==> Dev logs\033[0m\n'
	$(DEV_COMPOSE) logs --follow

down: ## Stop the dev stack
	@printf '\033[1;34m==> Stopping dev stack\033[0m\n'
	$(DEV_COMPOSE) down

reset: ## Delete dev data and volumes after confirmation
	@printf '\033[1;33mType reset to delete .dev/ and dev volumes: \033[0m'; \
	if IFS= read -r answer && [ "$$answer" = reset ]; then \
		$(DEV_COMPOSE) down --volumes && rm -rf .dev; \
	else \
		printf 'Reset cancelled.\n'; \
	fi

##@ Quality

lint: ## Run clippy (warnings are errors) and check formatting
	@printf '\033[1;34m==> clippy\033[0m\n'
	cargo clippy $(CARGO_SCOPE) --all-targets --locked -- -D warnings
	cargo clippy --locked -p rusttalk-bot --no-default-features --all-targets -- -D warnings
	@printf '\033[1;34m==> rustfmt --check\033[0m\n'
	cargo fmt --all --check

fmt: ## Format all code
	cargo fmt --all

test: ## Run unit and integration tests
	@printf '\033[1;34m==> tests\033[0m\n'
	cargo test $(CARGO_SCOPE) --locked
	PATH="$(OPENSSL_PATH)$$PATH" python3 -m unittest discover -s deploy/updater

.PHONY: android-camera-test
android-camera-test: ## Run deterministic Android camera lifecycle tests (JDK, no device)
	@set -eu; work=$$(mktemp -d); trap 'rm -rf "$$work"' EXIT; \
		javac -d "$$work" deploy/android/src/main/java/io/github/rusttalkcrew/rusttalk/CameraLifecycle.java \
			deploy/android/tests/CameraLifecycleTest.java; \
		java -ea -cp "$$work" io.github.rusttalkcrew.rusttalk.CameraLifecycleTest

e2e: ## Run the two-stack membership, keys, chat, stored files, voice, app audio, screen, camera, live file, device sync and index scenarios
	@printf '\033[1;34m==> Two-stack membership, keys, chat, stored files, voice, app audio, screen, camera, live file, device sync and index\033[0m\n'
	$(if $(and $(PIPEWIRE_HELPER),$(findstring media,$(E2E_FEATURES))),cargo build --locked $(PIPEWIRE_HELPER))
	@set -eu; \
		export RUSTTALK_E2E_PROJECT=rusttalk-e2e-$$$$ RUSTTALK_INDEX_TTL_SECONDS=8; \
		export RUSTTALK_INDEX_ADMIN_TOKEN=$$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n') RUSTTALK_INDEX_CONTACT=directory@example.org; \
		export RUSTTALK_DEV_DATA=$$(mktemp -d); \
		export RUSTTALK_E2E_DB=$$(mktemp -d /dev/shm/rusttalk-e2e.XXXXXX); \
		for stack in "$$RUSTTALK_E2E_PROJECT" "$$RUSTTALK_E2E_PROJECT-second"; do \
			mkdir "$$RUSTTALK_E2E_DB/$$stack-api" "$$RUSTTALK_E2E_DB/$$stack-index"; \
		done; \
		export RUSTTALK_E2E_SECOND_PROJECT="$$RUSTTALK_E2E_PROJECT-second"; \
		export RUSTTALK_E2E_SECOND_HTTPS_PORT="$${RUSTTALK_E2E_SECOND_HTTPS_PORT:-9443}"; \
		export RUSTTALK_FILE_MAX_BYTES=33554432 RUSTTALK_FILE_MEMBER_BYTES=3145728 RUSTTALK_FILE_SERVER_BYTES=5242880 RUSTTALK_FILE_TTL_SECS=12 RUSTTALK_STICKER_MAX=1; \
		compose() { docker compose -f deploy/compose.dev.yml -f deploy/compose.e2e.yml --profile index -p "$$RUSTTALK_E2E_PROJECT" "$$@"; }; \
		compose_second() { \
			RUSTTALK_DEV_DATA="$$RUSTTALK_E2E_SECOND_DATA" \
			RUSTTALK_DEV_HTTPS_PORT="$$RUSTTALK_E2E_SECOND_HTTPS_PORT" \
			RUSTTALK_DEV_MEDIA_TCP_PORT="$${RUSTTALK_E2E_SECOND_MEDIA_TCP_PORT:-8881}" \
			RUSTTALK_DEV_MEDIA_UDP_PORT="$${RUSTTALK_E2E_SECOND_MEDIA_UDP_PORT:-8882}" \
			RUSTTALK_DEV_NEW_DMS_PER_HOUR=2 RUSTTALK_FILE_MAX_BYTES= RUSTTALK_FILE_MEMBER_BYTES= RUSTTALK_FILE_SERVER_BYTES= RUSTTALK_FILE_TTL_SECS= RUSTTALK_STICKER_MAX= \
			docker compose -f deploy/compose.dev.yml -f deploy/compose.e2e.yml -p "$$RUSTTALK_E2E_SECOND_PROJECT" "$$@"; \
		}; \
		cleanup() { \
			status=$$?; trap - EXIT; \
			if [ -n "$${RUSTTALK_E2E_SECOND_DATA:-}" ]; then \
				compose_second down --volumes --rmi local || status=1; \
				rm -rf "$$RUSTTALK_E2E_SECOND_DATA"; \
			fi; \
			compose down --volumes --rmi local || status=1; \
			rm -rf "$$RUSTTALK_DEV_DATA" "$$RUSTTALK_E2E_DB"; exit "$$status"; \
		}; \
		trap cleanup EXIT; trap 'exit 130' INT; trap 'exit 143' TERM; \
		export RUSTTALK_E2E_SECOND_DATA=$$(mktemp -d); \
		compose up --build --wait --wait-timeout 60; \
		compose_second up --build --wait --wait-timeout 60; \
		cargo test --locked -p rusttalk-bot --no-default-features $(E2E_FEATURES) --test membership -- --nocapture

soak: ## Run two headless bots against a disposable stack for 30 minutes
	@bash deploy/soak.sh

##@ Release

release-key: ## Generate an Ed25519 key pair in .dev for local update tests
	@printf '\033[1;34m==> Generating local test release key\033[0m\n'
	python3 deploy/release/release.py key

release: ## Tag origin/main as v<Cargo.toml version> and push it; CI builds, signs and publishes
	@set -e; version=$$(python3 deploy/release/release.py version); \
		test -z "$$(git status --porcelain)" || { printf '\033[1;31mCommit or stash your changes first.\033[0m\n' >&2; exit 1; }; \
		git fetch -q origin main; test "$$(git rev-parse HEAD)" = "$$(git rev-parse origin/main)" || { printf '\033[1;31mCheck out the latest origin/main first.\033[0m\n' >&2; exit 1; }; \
		printf '\033[1;34m==> Tagging v%s\033[0m\n' "$$version"; \
		git tag -s "v$$version" -m "RustTalk $$version"; git push origin "v$$version"; \
		printf '\033[1;34m==> Building in CI: https://github.com/rusttalkcrew/rusttalk-next/actions/workflows/release.yml\033[0m\n'
