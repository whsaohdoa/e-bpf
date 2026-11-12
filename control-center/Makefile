# Convenience entry points. The scripts are the source of truth: this Makefile only wraps
# them so that `make up` from the plan's acceptance script works on any machine with make.
# On Windows without make, call the same scripts through Git Bash: bash scripts/up.sh
SHELL := /bin/bash

.PHONY: help up down wipe smoke p95 audit-check migrate psql k3d-up k3d-down k3d-status test clean

help:
	@echo "make up            build and start the compose stack (pg + mgmt + control + web)"
	@echo "make down          stop containers, keep the database volume"
	@echo "make wipe          stop containers and delete the database volume"
	@echo "make smoke         assert the SPA proxy, both roles and the seed data"
	@echo "make p95 [N=50]    latency sample against /api/health"
	@echo "make audit-check   prove mgmt.audit is append-only"
	@echo "make migrate DB=mgmt N=1   roll back N changesets (demo of down)"
	@echo "make psql DB=mgmt  interactive psql as cc_app"
	@echo "make k3d-up        same stack on a k3d cluster (port 8080, compose must be down)"
	@echo "make k3d-down      delete the k3d cluster"
	@echo "make k3d-status    pods, services and ingress"
	@echo "make test          mvn test for the service (needs network on first run)"

up:
	bash scripts/up.sh

down:
	bash scripts/down.sh

wipe:
	bash scripts/down.sh --wipe

smoke:
	bash scripts/smoke.sh

p95:
	bash scripts/p95.sh $(or $(N),50)

audit-check:
	bash scripts/audit-check.sh

migrate:
	bash scripts/migrate.sh $(or $(DB),mgmt) $(or $(N),1)

psql:
	bash scripts/psql.sh $(or $(DB),mgmt)

k3d-up:
	bash scripts/k3d-up.sh

k3d-down:
	bash scripts/k3d-down.sh

k3d-status:
	kubectl -n cc-train0 get pods,svc,ingress

test:
	docker run --rm --network host -v "$(CURDIR)/app:/app" -w /app \
		-e MAVEN_CONFIG=/tmp/.m2 \
		$(or $(CC_MAVEN_IMAGE),docker.1ms.run/library/maven:3.9-eclipse-temurin-21) \
		mvn -B -ntp test

clean:
	rm -rf app/target web/dist web/node_modules
