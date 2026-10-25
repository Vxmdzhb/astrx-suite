# astrx-suite — deployment convenience targets.
#
# Works from anywhere: it locates the suite-root docker-compose.yml relative to
# this Makefile, so `make -C deploy up` and `cd deploy && make up` both work.

HERE    := $(patsubst %/,%,$(dir $(abspath $(lastword $(MAKEFILE_LIST)))))
ROOT    := $(abspath $(HERE)/..)
COMPOSE := docker compose -f $(ROOT)/docker-compose.yml

.DEFAULT_GOAL := help
.PHONY: help config build up down restart ps logs onions verify-tor \
        crawl check test clean

help: ## Show this help.
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) \
	  | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-14s\033[0m %s\n",$$1,$$2}'

config: ## Validate + render the fully-resolved compose config.
	$(COMPOSE) config

build: ## Build the suite and tor images (one cargo build).
	$(COMPOSE) build

up: ## Build (if needed) and start the published stack in the background.
	$(COMPOSE) up -d

down: ## Stop and remove containers + networks (volumes are kept).
	$(COMPOSE) down

restart: ## Recreate the running stack.
	$(COMPOSE) up -d --force-recreate

ps: ## Show the stack's containers.
	$(COMPOSE) ps

logs: ## Follow logs (make logs S=serve to scope to one container; lines carry [role]).
	$(COMPOSE) logs -f $(S)

onions: ## Print the generated .onion address of every published service.
	@$(COMPOSE) exec -T tor sh -c \
	  'for f in /var/lib/tor/*/hostname; do \
	     printf "%-26s %s\n" "$$(basename $$(dirname $$f))" "$$(cat $$f)"; done'

verify-tor: ## Syntax-check torrc inside the tor image.
	$(COMPOSE) run --rm tor --verify-config

crawl: ## Start the egress container (its roles: ASTRX_CRAWL_ROLES in .env).
	COMPOSE_PROFILES=crawl $(COMPOSE) up -d crawl

check: ## Ask suitedash whether every front-end is UP (exit non-zero if not).
	$(COMPOSE) exec -T serve /usr/local/bin/astrx suitedash --check \
	  --service gitweb=http://127.0.0.1:8801 --service onioncrawler=http://127.0.0.1:8802 \
	  --service websearch=http://127.0.0.1:8803 --service torrentds=http://127.0.0.1:8804

test: ## Run the full workspace test suite + lint gate on the host.
	cd $(ROOT) && bash install_rust_suite.sh --ci

clean: ## Remove containers, networks AND volumes (DESTROYS the .onion keys).
	$(COMPOSE) down -v
