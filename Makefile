.DEFAULT_GOAL := help
.PHONY: help dev bot logs down reset lint fmt test

DEV_COMPOSE := docker compose -f deploy/compose.dev.yml -p rusttalk-dev
CARGO_SCOPE ?= --workspace

##@ Help

help: ## Show this help
	@awk 'BEGIN {FS = ":.*## "} \
		/^##@/ {printf "\n\033[1;33m%s\033[0m\n", substr($$0, 5)} \
		/^[a-zA-Z_-]+:.*##/ {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo

##@ Dev

dev: export RUSTTALK_DEV_USER = $(shell id -u):$(shell id -g)
dev: export NODE_IP ?= $(shell ip -4 route get 1.1.1.1 2>/dev/null | awk '{ for (i = 1; i < NF; i++) if ($$i == "src") { print $$(i + 1); exit } }')
dev: ## Start the Docker stack and rebuild the API on changes
	@test -n "$$NODE_IP" || { printf '\033[1;31mSet NODE_IP to the host LAN IPv4 address.\033[0m\n' >&2; exit 1; }
	@mkdir -p .dev
	@printf '\033[1;34m==> Starting dev stack\033[0m\n'
	$(DEV_COMPOSE) up --build --wait --wait-timeout 60
	@fingerprint=$$($(DEV_COMPOSE) run --rm --no-deps -T --entrypoint openssl cert \
		x509 -in /dev-data/tls/cert.pem -noout -sha256 -fingerprint) && \
		printf '\033[1;34m==> https://127.0.0.1:8443\n==> LAN wss://%s:8443\n==> TLS %s\033[0m\n' "$$NODE_IP" "$$fingerprint"
	$(DEV_COMPOSE) watch --no-up api

bot: export CARGO_TARGET_DIR ?= target
bot: ## Join the dev room as bot-a and bot-b
	@printf '\033[1;34m==> Starting dev bots\033[0m\n'
	cargo build --locked -p rusttalk-bot
	@bash -ec '"$$CARGO_TARGET_DIR/debug/rusttalk-bot" --identity bot-a & first=$$!; \
		"$$CARGO_TARGET_DIR/debug/rusttalk-bot" --identity bot-b & second=$$!; \
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
