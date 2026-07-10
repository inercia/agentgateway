# Paths relative to this Makefile so `make -C adobe` and `cd adobe && make` both work.
ADOBE_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
REPO_ROOT := $(abspath $(ADOBE_DIR)/..)
# Adobe snapshot/release builds use adobe/Dockerfile (sccache); root Dockerfile matches upstream.
ADOBE_DOCKERFILE := $(ADOBE_DIR)Dockerfile


ADOBE_VERSION ?= $(shell cat $(ADOBE_DIR)version)
PUBLIC_REMOTE ?= https://github.com/agentgateway/agentgateway.git
BUILD_FEATURES ?= agentgateway/ui,agentgateway/adobe
PLATFORM ?= linux/amd64
BUILD_CACHE_DIR ?= $(HOME)/.cache/agentgateway-buildx
# docker-container driver is required for --cache-to; Docker Desktop default is "docker".
BUILDX_BUILDER_NAME ?= agentgateway-builder
# Resolve upstream version from tags. Only consider upstream semver tags
# (`v*`) so that local Adobe release tags (`agw-*`, `api/*`) do not get
# picked up by `git describe`. If git describe fails to find a tag, fetch
# tags from the public repo and retry exactly once.
UPSTREAM_VERSION ?= $(shell \
	v=$$(git describe --tags --match 'v*' --always --dirty 2>/dev/null); \
	if echo "$$v" | grep -q '^v'; then echo "$$v"; else \
		git fetch $(PUBLIC_REMOTE) --tags >/dev/null 2>&1; \
		git describe --tags --match 'v*' --always --dirty 2>/dev/null; \
	fi)
GIT_REVISION ?= $(shell git rev-parse HEAD)

# Pin VERSION for iterative rebuilds without re-resolving tags: VERSION=dev-local make snapshot
VERSION ?= $(UPSTREAM_VERSION)

SNAPSHOT_REGISTRY = docker-api-platform-snapshot.dr-uw2.adobeitc.com
RELEASE_REGISTRY = docker-api-platform-release.dr-uw2.adobeitc.com
IMAGE_REPO = apiplatform/v2
IMAGE_NAME = agentgateway

ARCH = $(subst linux/,,$(PLATFORM))
SNAPSHOT_TAG = $(UPSTREAM_VERSION)-adobe-$(ADOBE_VERSION)-$(ARCH)
RELEASE_TAG = $(UPSTREAM_VERSION)-adobe-$(ADOBE_VERSION)-$(ARCH)

SNAPSHOT_IMAGE = $(SNAPSHOT_REGISTRY)/$(IMAGE_REPO)/$(IMAGE_NAME):$(SNAPSHOT_TAG)
RELEASE_IMAGE = $(RELEASE_REGISTRY)/$(IMAGE_REPO)/$(IMAGE_NAME):$(RELEASE_TAG)

DOCKER_BUILDX_CACHE = \
	--cache-from type=local,src=$(BUILD_CACHE_DIR) \
	--cache-to type=local,dest=$(BUILD_CACHE_DIR),mode=max

DOCKER_BUILDX = docker buildx build \
	-f $(ADOBE_DOCKERFILE) \
	--builder $(BUILDX_BUILDER_NAME) \
	--platform $(PLATFORM) \
	--build-arg BUILDER=musl \
	--build-arg VERSION=$(VERSION) \
	--build-arg GIT_REVISION=$(GIT_REVISION) \
	--build-arg CARGO_FEATURES=$(BUILD_FEATURES) \
	--build-arg PROFILE=$(PROFILE) \
	$(DOCKER_BUILDX_CACHE) \
	--load \
	--progress=plain

# snapshot/release use buildx local cache; bootstrap builder on first run: make setup-buildx
.PHONY: setup-buildx ensure-buildx-builder check-version

setup-buildx: ensure-buildx-builder

ensure-buildx-builder:
	@if ! docker buildx inspect $(BUILDX_BUILDER_NAME) >/dev/null 2>&1; then \
		echo "Creating buildx builder '$(BUILDX_BUILDER_NAME)' (docker-container driver) for cache export..."; \
		docker buildx create --name $(BUILDX_BUILDER_NAME) --driver docker-container --bootstrap; \
	fi
	@docker buildx use $(BUILDX_BUILDER_NAME) >/dev/null
	@docker buildx inspect --bootstrap $(BUILDX_BUILDER_NAME) >/dev/null

check-version:
	@case "$(UPSTREAM_VERSION)" in \
		v*) ;; \
		*) echo "ERROR: UPSTREAM_VERSION '$(UPSTREAM_VERSION)' does not start with 'v'." >&2; \
		   echo "       Tags could not be resolved. Run 'make fetch-tags' or check network." >&2; \
		   exit 1 ;; \
	esac

.PHONY: snapshot release

# snapshot: quick-release + sccache + buildx cache (default). Prod-like: PROFILE=release make snapshot or make release
snapshot: override PROFILE := $(if $(filter command line environment,$(origin PROFILE)),$(PROFILE),quick-release)
snapshot: check-version ensure-buildx-builder
	$(DOCKER_BUILDX) -t $(SNAPSHOT_IMAGE) $(REPO_ROOT)
	@echo ""
	@echo "Snapshot image: $(SNAPSHOT_IMAGE)"
	@digest=$$(docker image inspect $(SNAPSHOT_IMAGE) --format '{{if .RepoDigests}}{{index .RepoDigests 0}}{{else}}{{.Id}}{{end}}' 2>/dev/null); \
		if [ -n "$$digest" ]; then echo "Snapshot digest: $$digest"; fi

release: override PROFILE := $(if $(filter command line environment,$(origin PROFILE)),$(PROFILE),release)
release: check-version ensure-buildx-builder
	$(DOCKER_BUILDX) -t $(RELEASE_IMAGE) $(REPO_ROOT)
	@echo ""
	@echo "Release image: $(RELEASE_IMAGE)"
	@digest=$$(docker image inspect $(RELEASE_IMAGE) --format '{{if .RepoDigests}}{{index .RepoDigests 0}}{{else}}{{.Id}}{{end}}' 2>/dev/null); \
		if [ -n "$$digest" ]; then echo "Release digest: $$digest"; fi

.PHONY: fetch-tags
fetch-tags:
	@git fetch $(PUBLIC_REMOTE) --tags

.PHONY: print-snapshot-image print-release-image
print-snapshot-image: check-version
	@echo $(SNAPSHOT_IMAGE)

print-release-image: check-version
	@echo $(RELEASE_IMAGE)

# --- Release tags (both derived from adobe/version) ---
TAG_REF      ?= HEAD
PUSH         ?= 1
RELEASE      ?= 1
RELEASE_REPO ?= Adobe-Apis/agentgateway
CHANGELOG    := $(ADOBE_DIR)CHANGELOG.md
# Strip stray GITHUB_TOKEN/GH_TOKEN so gh uses keyring / GHEC login (see sync skill).
GH           ?= env -u GITHUB_TOKEN -u GH_TOKEN gh
API_TAG      := api/v$(ADOBE_VERSION)
GATEWAY_TAG   = agw-$(UPSTREAM_VERSION)-adobe-$(ADOBE_VERSION)

.PHONY: print-release-tags cut-release-tags create-release print-release-notes

print-release-tags: check-version
	@echo "$(GATEWAY_TAG)"
	@echo "$(API_TAG)"

print-release-notes: check-version
	@awk -v ver="$(ADOBE_VERSION)" 'BEGIN{f=0} $$0=="## " ver{f=1;next} /^## /&&f{f=0} f{print}' "$(CHANGELOG)"
	@prev=$$(git tag --list 'agw-*' --sort=-creatordate | grep -vxF "$(GATEWAY_TAG)" | head -n1); \
	if [ -n "$$prev" ]; then printf '\n**Full Changelog**: https://github.com/$(RELEASE_REPO)/compare/%s...%s\n' "$$prev" "$(GATEWAY_TAG)"; fi

create-release: check-version
	@command -v $(firstword $(GH)) >/dev/null 2>&1 || { echo "ERROR: gh CLI not found." >&2; exit 1; }
	@git ls-remote --exit-code --tags origin "refs/tags/$(GATEWAY_TAG)" >/dev/null 2>&1 || { \
		echo "ERROR: $(GATEWAY_TAG) is not on origin; push it before creating a release." >&2; exit 1; }
	@if $(GH) release view "$(GATEWAY_TAG)" --repo "$(RELEASE_REPO)" >/dev/null 2>&1; then \
		echo "Skipping GitHub release $(GATEWAY_TAG) (already exists)."; exit 0; fi
	@section=$$(awk -v ver="$(ADOBE_VERSION)" 'BEGIN{f=0} $$0=="## " ver{f=1;next} /^## /&&f{f=0} f{print}' "$(CHANGELOG)"); \
	if [ -z "$$(printf '%s' "$$section" | tr -d '[:space:]')" ]; then \
		echo "ERROR: no '## $(ADOBE_VERSION)' section in $(CHANGELOG)." >&2; exit 1; fi; \
	notes=$$(mktemp); printf '%s\n' "$$section" > "$$notes"; \
	prev=$$(git tag --list 'agw-*' --sort=-creatordate | grep -vxF "$(GATEWAY_TAG)" | head -n1); \
	if [ -n "$$prev" ]; then printf '\n**Full Changelog**: https://github.com/$(RELEASE_REPO)/compare/%s...%s\n' "$$prev" "$(GATEWAY_TAG)" >> "$$notes"; fi; \
	$(GH) release create "$(GATEWAY_TAG)" --repo "$(RELEASE_REPO)" --title "$(GATEWAY_TAG)" --notes-file "$$notes"; \
	rc=$$?; rm -f "$$notes"; \
	if [ $$rc -eq 0 ]; then echo "Created GitHub release $(GATEWAY_TAG)."; fi; \
	exit $$rc

cut-release-tags: check-version
	@case "$(ADOBE_VERSION)" in \
		[0-9]*.[0-9]*.[0-9]*) ;; \
		*) echo "ERROR: adobe/version '$(ADOBE_VERSION)' is not X.Y.Z semver." >&2; exit 1 ;; \
	esac
	@if [ -n "$$(git status --porcelain)" ]; then \
		echo "ERROR: working tree dirty; commit or stash before tagging." >&2; exit 1; fi
	@set -e; \
	tags_to_push=; \
	for t in "$(GATEWAY_TAG)" "$(API_TAG)"; do \
		if git ls-remote --exit-code --tags origin "refs/tags/$$t" >/dev/null 2>&1; then \
			echo "Skipping $$t (already on origin)."; \
		elif git rev-parse -q --verify "refs/tags/$$t" >/dev/null 2>&1; then \
			echo "Skipping create $$t (exists locally); will push."; \
			tags_to_push="$$tags_to_push $$t"; \
		else \
			case "$$t" in api/*) msg="agentgateway api $$t";; *) msg="agentgateway $$t";; esac; \
			git tag -a "$$t" $(TAG_REF) -m "$$msg"; \
			echo "Created $$t at $(TAG_REF)."; \
			tags_to_push="$$tags_to_push $$t"; \
		fi; \
	done; \
	if [ "$(PUSH)" = "1" ]; then \
		if [ -n "$$tags_to_push" ]; then \
			git push origin $$tags_to_push && echo "Pushed:$$tags_to_push"; \
		else echo "No new tags to push."; fi; \
		if [ "$(RELEASE)" = "1" ]; then $(MAKE) --no-print-directory create-release; \
		else echo "RELEASE=0 set; GitHub release skipped."; fi; \
	else \
		if [ -n "$$tags_to_push" ]; then \
			echo "PUSH=0 set; push later: git push origin$$tags_to_push"; \
		else echo "PUSH=0 set; all tags already exist on origin."; fi; \
		echo "(GitHub release skipped; after pushing run: make -C adobe create-release)"; \
	fi

# Full release: prod image build, then tag + push + GitHub release. No snapshot.
# Recursive $(MAKE) calls (not prerequisites) so each stage fully finishes
# before the next starts, same pattern cut-release-tags uses for create-release.
.PHONY: full-release
full-release: check-version
	@echo "==> Building release image"
	$(MAKE) --no-print-directory release
	@echo "==> Cutting release tags and GitHub release"
	$(MAKE) --no-print-directory cut-release-tags
