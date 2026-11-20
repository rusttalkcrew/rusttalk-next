.DEFAULT_GOAL := help
.PHONY: help dev client bot logs down reset lint fmt test e2e

DEV_COMPOSE := docker compose -f deploy/compose.dev.yml -p rusttalk-dev
export RUSTTALK_DEV_USER := $(shell id -u):$(shell id -g)
CARGO_SCOPE ?= --workspace
E2E_FEATURES ?= --features media
ifeq ($(shell uname -s),Linux)
PIPEWIRE_HELPER := -p rusttalk-pipewire
endif

##@ Help

help: ## Show this help
	@awk 'BEGIN {FS = ":.*## "} \
		/^##@/ {printf "\n\033[1;33m%s\033[0m\n", substr($$0, 5)} \
		/^[a-zA-Z0-9_-]+:.*##/ {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo

##@ Dev

dev: export NODE_IP ?= $(shell ip -4 route get 1.1.1.1 2>/dev/null | awk '{ for (i = 1; i < NF; i++) if ($$i == "src") { print $$(i + 1); exit } }')
dev: ## Start the Docker stack and rebuild the API on changes
	@test -n "$$NODE_IP" || { printf '\033[1;31mSet NODE_IP to the host LAN IPv4 address.\033[0m\n' >&2; exit 1; }
	@mkdir -p .dev
	@printf '\033[1;34m==> Starting dev stack\033[0m\n'
	$(DEV_COMPOSE) up --build --wait --wait-timeout 60
	@fingerprint=$$($(DEV_COMPOSE) run --rm --no-deps -T --entrypoint openssl cert \
		x509 -in /dev-data/tls/cert.pem -noout -sha256 -fingerprint) && \
		printf '\033[1;34m==> https://127.0.0.1:8443\n==> LAN wss://%s:8443\n==> TLS %s\033[0m\n' "$$NODE_IP" "$$fingerprint"
	@server=$$($(DEV_COMPOSE) exec -T api rusttalk-api server-fingerprint) && \
		printf '\033[1;34m==> Server fingerprint %s\033[0m\n' "$$server"
	@invite=$$($(DEV_COMPOSE) exec -T api rusttalk-api owner-invite) && \
		if [ -n "$$invite" ]; then \
			server_id=$$($(DEV_COMPOSE) exec -T api rusttalk-api server-id) && \
			tls_pin=$$($(DEV_COMPOSE) run --rm --no-deps -T --entrypoint /bin/sh cert -ec \
				'set -o pipefail; openssl x509 -in /dev-data/tls/cert.pem -outform DER | openssl dgst -sha256 -binary | openssl base64 -A | tr "/+" "_-" | tr -d "="') && \
			printf '\033[1;34m==> Owner invite rusttalk://127.0.0.1:8443/i/%s#%s.%s\033[0m\n' "$$invite" "$$server_id" "$$tls_pin"; \
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
	@printf '\033[1;34m==> rustfmt --check\033[0m\n'
	cargo fmt --all --check

fmt: ## Format all code
	cargo fmt --all

test: ## Run unit and integration tests
	@printf '\033[1;34m==> tests\033[0m\n'
	cargo test $(CARGO_SCOPE) --locked

e2e: ## Run the Docker membership, keys, encrypted chat, media and device sync scenarios
	@printf '\033[1;34m==> Docker membership, keys, encrypted chat, media and device sync\033[0m\n'
	$(if $(and $(PIPEWIRE_HELPER),$(findstring media,$(E2E_FEATURES))),cargo build --locked $(PIPEWIRE_HELPER))
	@set -eu; \
		export RUSTTALK_E2E_PROJECT=rusttalk-e2e-$$$$; \
		export RUSTTALK_DEV_DATA=$$(mktemp -d); \
		compose() { docker compose -f deploy/compose.dev.yml -p "$$RUSTTALK_E2E_PROJECT" "$$@"; }; \
		cleanup() { \
			status=$$?; trap - EXIT; \
			compose down --volumes --rmi local || status=1; \
			rm -rf "$$RUSTTALK_DEV_DATA"; exit "$$status"; \
		}; \
		trap cleanup EXIT; trap 'exit 130' INT; trap 'exit 143' TERM; \
		compose up --build --wait --wait-timeout 60; \
		cargo test --locked -p rusttalk-bot --no-default-features $(E2E_FEATURES) --test membership -- --nocapture
