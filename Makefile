.DEFAULT_GOAL := help
.PHONY: help lint fmt test

##@ Help

help: ## Show this help
	@awk 'BEGIN {FS = ":.*## "} \
		/^##@/ {printf "\n\033[1;33m%s\033[0m\n", substr($$0, 5)} \
		/^[a-zA-Z_-]+:.*##/ {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo

##@ Quality

lint: ## Run clippy (warnings are errors) and check formatting
	@printf '\033[1;34m==> clippy\033[0m\n'
	cargo clippy --workspace --all-targets --locked -- -D warnings
	@printf '\033[1;34m==> rustfmt --check\033[0m\n'
	cargo fmt --all --check

fmt: ## Format all code
	cargo fmt --all

test: ## Run unit and integration tests
	@printf '\033[1;34m==> tests\033[0m\n'
	cargo test --workspace --locked
